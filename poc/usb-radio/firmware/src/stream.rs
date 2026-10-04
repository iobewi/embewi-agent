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
use usb_radio_core::{FileReadStatus, FileSource, SECTOR_SIZE};

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        CELL.uninit().write($val)
    }};
}
use mk_static;

pub const STREAM_URL: &str =
    "http://icecast.radiofrance.fr/monpetitfranceinter-midfi.mp3";

const RING_CAPACITY: usize = 96 * 1024;
const PREBUFFER_BYTES: u64 = 64 * 1024;
const MAX_LEAD_BYTES: u64 = 80 * 1024;

pub static STREAM: SharedStream = SharedStream::new();

pub struct SharedStream {
    inner: Mutex<CriticalSectionRawMutex, RefCell<StreamState>>,
}

#[derive(Clone, Copy)]
struct Session {
    id: u32,
    base_abs: u64,
    read_end_abs: u64,
}

struct StreamState {
    data: [u8; RING_CAPACITY],
    write_abs: u64,
    reconnects: u32,
    next_session_id: u32,
    session: Option<Session>,
}

impl SharedStream {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(RefCell::new(StreamState {
                data: [0; RING_CAPACITY],
                write_abs: 0,
                reconnects: 0,
                next_session_id: 1,
                session: None,
            })),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.inner
            .lock(|cell| cell.borrow().write_abs >= PREBUFFER_BYTES)
    }

    pub fn progress(&self) -> (u64, u64) {
        self.inner.lock(|cell| {
            let state = cell.borrow();
            let consumed = state
                .session
                .map(|session| session.read_end_abs)
                .unwrap_or(0);
            (state.write_abs, consumed)
        })
    }

    pub fn begin_session(&self) {
        self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();
            let retained = core::cmp::min(state.write_abs, PREBUFFER_BYTES);
            let base_abs = state.write_abs - retained;
            let id = state.next_session_id;
            state.next_session_id = state.next_session_id.wrapping_add(1).max(1);
            state.session = Some(Session {
                id,
                base_abs,
                read_end_abs: base_abs,
            });

            esp_println::println!(
                "stream: session start id={} base={} live={} retained={}",
                id,
                base_abs,
                state.write_abs,
                retained
            );
        });
    }

    pub fn end_session(&self) {
        self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();
            if let Some(session) = state.session.take() {
                esp_println::println!(
                    "stream: session end id={} base={} consumed={} live={}",
                    session.id,
                    session.base_abs,
                    session.read_end_abs,
                    state.write_abs
                );
            }
        });
    }

    fn writable(&self) -> usize {
        self.inner.lock(|cell| {
            let state = cell.borrow();
            match state.session {
                None => 2048,
                Some(session) => {
                    let lead = state.write_abs.saturating_sub(session.read_end_abs);
                    core::cmp::min(
                        MAX_LEAD_BYTES.saturating_sub(lead) as usize,
                        2048,
                    )
                }
            }
        })
    }

    fn push(&self, input: &[u8]) -> usize {
        self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();

            let allowed = match state.session {
                None => input.len(),
                Some(session) => {
                    let lead = state.write_abs.saturating_sub(session.read_end_abs);
                    core::cmp::min(
                        input.len(),
                        MAX_LEAD_BYTES.saturating_sub(lead) as usize,
                    )
                }
            };

            if allowed == 0 {
                return 0;
            }

            let pos = state.write_abs as usize % RING_CAPACITY;
            let first = core::cmp::min(allowed, RING_CAPACITY - pos);
            state.data[pos..pos + first].copy_from_slice(&input[..first]);
            if first < allowed {
                state.data[..allowed - first].copy_from_slice(&input[first..allowed]);
            }
            state.write_abs += allowed as u64;
            allowed
        })
    }

    fn mark_reconnect(&self) -> u32 {
        self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();
            state.reconnects += 1;
            state.reconnects
        })
    }

    fn read_file_sector(
        &self,
        index: u32,
        out: &mut [u8; SECTOR_SIZE],
    ) -> FileReadStatus {
        self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();
            let Some(mut session) = state.session else {
                out.fill(0);
                return FileReadStatus::Pending;
            };

            let file_offset = index as u64 * SECTOR_SIZE as u64;
            let start = session.base_abs.saturating_add(file_offset);
            let end = start.saturating_add(SECTOR_SIZE as u64);

            if end > state.write_abs {
                out.fill(0);
                return FileReadStatus::Pending;
            }

            let oldest = state.write_abs.saturating_sub(RING_CAPACITY as u64);
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

            session.read_end_abs = core::cmp::max(session.read_end_abs, end);
            state.session = Some(session);
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
    fn begin_session(&mut self) {
        self.stream.begin_session();
    }

    fn end_session(&mut self) {
        self.stream.end_session();
    }

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
        let reconnect = stream.mark_reconnect();
        esp_println::println!(
            "stream: connecting attempt={} url={}",
            reconnect,
            STREAM_URL
        );

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

        // reqwless already provides Host from the URL.
        let mut request = request.headers(&[
            ("Connection", "close"),
            ("Icy-MetaData", "0"),
            ("User-Agent", "usb-radio-poc/0.3"),
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
                Timer::after(Duration::from_millis(5)).await;
            }

            let want = core::cmp::min(stream.writable(), buf.len());
            if want == 0 {
                continue;
            }

            match body.read(&mut buf[..want]).await {
                Ok(0) => {
                    esp_println::println!("stream: server closed connection");
                    break;
                }
                Ok(n) => {
                    let before = stream.progress().0;
                    let pushed = stream.push(&buf[..n]);
                    let (written, consumed) = stream.progress();

                    let before_bucket = before / (64 * 1024);
                    let after_bucket = written / (64 * 1024);
                    if before < PREBUFFER_BYTES && written >= PREBUFFER_BYTES
                        || after_bucket != before_bucket
                    {
                        esp_println::println!(
                            "stream: live={} consumed={} lead={}",
                            written,
                            consumed,
                            written.saturating_sub(consumed)
                        );
                    }

                    if pushed == 0 {
                        Timer::after(Duration::from_millis(5)).await;
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
