use core::cell::RefCell;

use embassy_net::{
    Stack,
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Duration, Timer};
use embedded_io_async::Read as _;
use reqwless::{
    client::HttpClient,
    request::{Method, RequestBuilder},
};
use usb_radio_core::{FILE_SIZE, FileReadStatus, FileSource, SECTOR_SIZE};

// Stable alternative to static_cell::make_static! for the ESP toolchain used by this POC.
macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        CELL.uninit().write($val)
    }};
}
use mk_static;

pub const STREAM_URL: &str =
    "http://icecast.radiofrance.fr/monpetitfranceinter-midfi.mp3";
const STREAM_HOST: &str = "icecast.radiofrance.fr";

const RING_CAPACITY: usize = 96 * 1024;
const PREBUFFER_BYTES: u32 = 64 * 1024;
const MAX_LEAD_BYTES: u32 = 80 * 1024;

pub static STREAM: SharedStream = SharedStream::new();

pub struct SharedStream {
    inner: Mutex<CriticalSectionRawMutex, RefCell<StreamState>>,
}

struct StreamState {
    data: [u8; RING_CAPACITY],
    write_offset: u32,
    read_end: u32,
    reconnects: u32,
}

impl SharedStream {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(RefCell::new(StreamState {
                data: [0; RING_CAPACITY],
                write_offset: 0,
                read_end: 0,
                reconnects: 0,
            })),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.inner.lock(|cell| cell.borrow().write_offset >= PREBUFFER_BYTES)
    }

    pub fn progress(&self) -> (u32, u32) {
        self.inner.lock(|cell| {
            let state = cell.borrow();
            (state.write_offset, state.read_end)
        })
    }

    fn writable(&self) -> usize {
        self.inner.lock(|cell| {
            let state = cell.borrow();
            if state.write_offset >= FILE_SIZE {
                return 0;
            }

            let max_lead = if state.read_end == 0 {
                PREBUFFER_BYTES
            } else {
                MAX_LEAD_BYTES
            };
            let lead = state.write_offset.saturating_sub(state.read_end);
            let lead_room = max_lead.saturating_sub(lead);
            let file_room = FILE_SIZE - state.write_offset;
            core::cmp::min(lead_room, file_room) as usize
        })
    }

    fn push(&self, input: &[u8]) -> usize {
        self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();

            let max_lead = if state.read_end == 0 {
                PREBUFFER_BYTES
            } else {
                MAX_LEAD_BYTES
            };
            let lead = state.write_offset.saturating_sub(state.read_end);
            let allowed = core::cmp::min(
                max_lead.saturating_sub(lead),
                FILE_SIZE.saturating_sub(state.write_offset),
            ) as usize;
            let len = core::cmp::min(input.len(), allowed);
            if len == 0 {
                return 0;
            }

            let pos = state.write_offset as usize % RING_CAPACITY;
            let first = core::cmp::min(len, RING_CAPACITY - pos);
            state.data[pos..pos + first].copy_from_slice(&input[..first]);
            if first < len {
                state.data[..len - first].copy_from_slice(&input[first..len]);
            }
            state.write_offset += len as u32;
            len
        })
    }

    fn mark_reconnect(&self) -> u32 {
        self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();
            state.reconnects += 1;
            state.reconnects
        })
    }

    fn read_file_sector(&self, index: u32, out: &mut [u8; SECTOR_SIZE]) -> FileReadStatus {
        let start = index.saturating_mul(SECTOR_SIZE as u32);
        let end = start.saturating_add(SECTOR_SIZE as u32);

        self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();

            if end > state.write_offset {
                out.fill(0);
                return FileReadStatus::Pending;
            }

            let oldest = state.write_offset.saturating_sub(RING_CAPACITY as u32);
            if start < oldest {
                out.fill(0);
                return FileReadStatus::Expired;
            }

            let pos = start as usize % RING_CAPACITY;
            let first = core::cmp::min(SECTOR_SIZE, RING_CAPACITY - pos);
            out[..first].copy_from_slice(&state.data[pos..pos + first]);
            if first < SECTOR_SIZE {
                out[first..].copy_from_slice(&state.data[..SECTOR_SIZE - first]);
            }

            state.read_end = core::cmp::max(state.read_end, end);
            FileReadStatus::Ready
        })
    }
}

#[derive(Clone, Copy)]
pub struct SharedStreamSource {
    stream: &'static SharedStream,
}

impl SharedStreamSource {
    pub const fn new(stream: &'static SharedStream) -> Self {
        Self { stream }
    }
}

impl FileSource for SharedStreamSource {
    fn read_file_sector(
        &mut self,
        index: u32,
        out: &mut [u8; SECTOR_SIZE],
    ) -> FileReadStatus {
        self.stream.read_file_sector(index, out)
    }
}

pub async fn run(stack: Stack<'static>, stream: &'static SharedStream) -> ! {
    let tcp_state = mk_static!(
        TcpClientState<1, 4096, 4096>,
        TcpClientState::<1, 4096, 4096>::new()
    );
    let tcp_client = TcpClient::new(stack, tcp_state);
    let dns_client = DnsSocket::new(stack);

    stack.wait_config_up().await;
    if let Some(config) = stack.config_v4() {
        esp_println::println!("net: got IP {}", config.address);
    }

    loop {
        if stream.progress().0 >= FILE_SIZE {
            esp_println::println!("stream: virtual RADIO.MP3 is full");
            loop {
                Timer::after(Duration::from_secs(60)).await;
            }
        }

        let reconnect = stream.mark_reconnect();
        esp_println::println!("stream: connecting attempt={} url={}", reconnect, STREAM_URL);

        let mut client = HttpClient::new(&tcp_client, &dns_client);
        let mut header_buf = [0u8; 2048];

        let request = match client.request(Method::GET, STREAM_URL).await {
            Ok(request) => request,
            Err(err) => {
                esp_println::println!("stream: request error {:?}", err);
                Timer::after(Duration::from_secs(2)).await;
                continue;
            }
        };

        let mut request = request.headers(&[
            ("Host", STREAM_HOST),
            ("Connection", "close"),
            ("Icy-MetaData", "0"),
            ("User-Agent", "usb-radio-poc/0.2"),
        ]);

        let response = match request.send(&mut header_buf).await {
            Ok(response) => response,
            Err(err) => {
                esp_println::println!("stream: connect/send error {:?}", err);
                Timer::after(Duration::from_secs(2)).await;
                continue;
            }
        };

        esp_println::println!(
            "stream: HTTP status={} content_length={:?}",
            response.status.0,
            response.content_length
        );

        if response.status.0 != 200 {
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }

        let mut body = response.body().reader();
        let mut buf = [0u8; 2048];

        loop {
            while stream.writable() == 0 {
                if stream.progress().0 >= FILE_SIZE {
                    break;
                }
                Timer::after(Duration::from_millis(5)).await;
            }

            if stream.progress().0 >= FILE_SIZE {
                break;
            }

            let room = stream.writable();
            let want = core::cmp::min(room, buf.len());
            if want == 0 {
                continue;
            }

            match body.read(&mut buf[..want]).await {
                Ok(0) => {
                    esp_println::println!("stream: server closed connection");
                    break;
                }
                Ok(n) => {
                    let pushed = stream.push(&buf[..n]);
                    let (written, consumed) = stream.progress();
                    if written == PREBUFFER_BYTES || written % (64 * 1024) < pushed as u32 {
                        esp_println::println!(
                            "stream: buffered written={} consumed={} lead={}",
                            written,
                            consumed,
                            written.saturating_sub(consumed)
                        );
                    }
                }
                Err(err) => {
                    esp_println::println!("stream: body read error {:?}", err);
                    break;
                }
            }
        }

        Timer::after(Duration::from_secs(1)).await;
    }
}
