use core::cell::RefCell;
use iobewi_rolling_stream::{ReadStatus, RollingStream};

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
use usb_radio_core::{FileReadStatus, FileSource, SECTOR_SIZE, StreamWindow, classify_stream_read};

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        CELL.uninit().write($val)
    }};
}

pub const STREAM_URL: &str = "http://icecast.radiofrance.fr/monpetitfranceinter-midfi.mp3";

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
}

struct StreamState {
    window: RollingStream<RING_CAPACITY>,
    reconnects: u32,
    next_session_id: u32,
    session: Option<Session>,
}

impl SharedStream {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(RefCell::new(StreamState {
                window: RollingStream::new(PREBUFFER_BYTES, MAX_LEAD_BYTES),
                reconnects: 0,
                next_session_id: 1,
                session: None,
            })),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.inner
            .lock(|cell| cell.borrow().window.written() >= PREBUFFER_BYTES)
    }

    pub fn progress(&self) -> (u64, u64) {
        self.inner.lock(|cell| {
            let state = cell.borrow();
            let consumed = state.window.session().map(|s| s.read_end).unwrap_or(0);
            (state.window.written(), consumed)
        })
    }

    pub fn begin_session(&self) {
        self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();
            let retained = core::cmp::min(state.window.written(), PREBUFFER_BYTES);
            let base_abs = state.window.begin_session().base;
            let id = state.next_session_id;
            state.next_session_id = state.next_session_id.wrapping_add(1).max(1);
            state.session = Some(Session { id, base_abs });

            esp_println::println!(
                "stream: session start id={} base={} live={} retained={}",
                id,
                base_abs,
                state.window.written(),
                retained
            );
        });
    }

    pub fn end_session(&self) {
        self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();
            let ended = state.window.end_session();
            if let Some(session) = state.session.take() {
                esp_println::println!(
                    "stream: session end id={} base={} consumed={} live={}",
                    session.id,
                    session.base_abs,
                    ended.map(|s| s.read_end).unwrap_or(session.base_abs),
                    state.window.written()
                );
            }
        });
    }

    fn writable(&self) -> usize {
        self.inner
            .lock(|cell| cell.borrow().window.writable().min(2048))
    }

    fn push(&self, input: &[u8]) -> usize {
        self.inner.lock(|cell| cell.borrow_mut().window.push(input))
    }

    fn mark_reconnect(&self) -> u32 {
        self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();
            state.reconnects += 1;
            state.reconnects
        })
    }

    fn read_file_sector(&self, index: u32, out: &mut [u8; SECTOR_SIZE]) -> FileReadStatus {
        self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();
            let Some(session) = state.session else {
                out.fill(0);
                return FileReadStatus::Pending;
            };

            let file_offset = index as u64 * SECTOR_SIZE as u64;
            let start = session.base_abs.saturating_add(file_offset);
            let end = start.saturating_add(SECTOR_SIZE as u64);

            match classify_stream_read(start, end, state.window.written(), RING_CAPACITY as u64) {
                StreamWindow::FarAhead => {
                    // Probe far beyond the live edge (e.g. Windows reading the file tail):
                    // answer with zeroes now instead of blocking the MSC for hours.
                    out.fill(0);
                    return FileReadStatus::Ready;
                }
                StreamWindow::Pending => {
                    out.fill(0);
                    return FileReadStatus::Pending;
                }
                StreamWindow::Expired => {
                    out.fill(0);
                    return FileReadStatus::Expired;
                }
                StreamWindow::Ready => {}
            }

            match state.window.read(file_offset, out) {
                ReadStatus::Ready => FileReadStatus::Ready,
                ReadStatus::Pending => FileReadStatus::Pending,
                ReadStatus::Expired => FileReadStatus::Expired,
            }
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

    fn read_file_sector(&mut self, index: u32, out: &mut [u8; SECTOR_SIZE]) -> FileReadStatus {
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
