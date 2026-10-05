//! USB Radio identity and bounded unavailable-sector policy around IOBEWI MSC.

use embassy_time::Duration;
use embassy_usb::{Builder, driver::Driver};
use iobewi_usb_msc::{InquiryIdentity, ReadAction, ReadPolicy};
use usb_radio_core::{FileReadStatus, ReadOnlyBlockDevice};

pub use iobewi_usb_msc::{Error, State};

const PENDING_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_millis(5);
const IDENTITY: InquiryIdentity = InquiryIdentity {
    vendor: *b"IOBEWI  ",
    product: *b"USB RADIO POC   ",
    revision: *b"0001",
};

struct RadioReadPolicy;

impl ReadPolicy for RadioReadPolicy {
    fn unavailable(&mut self, status: FileReadStatus, elapsed: Duration) -> ReadAction {
        match status {
            FileReadStatus::Pending if elapsed < PENDING_TIMEOUT => {
                ReadAction::RetryAfter(RETRY_INTERVAL)
            }
            FileReadStatus::Pending => {
                esp_println::println!("msc: stream wait timed out");
                ReadAction::ZeroFill
            }
            FileReadStatus::Expired => {
                esp_println::println!("msc: stream data expired");
                ReadAction::ZeroFill
            }
            FileReadStatus::Ready => unreachable!("MSC policy only handles unavailable sectors"),
        }
    }
}

pub struct MscClass<'d, D: Driver<'d>> {
    class: iobewi_usb_msc::MscClass<'d, D>,
}

impl<'d, D: Driver<'d>> MscClass<'d, D> {
    pub fn new(builder: &mut Builder<'d, D>, state: &'d mut State<'d>) -> Self {
        Self {
            class: iobewi_usb_msc::MscClass::new(builder, state, IDENTITY),
        }
    }

    pub async fn run<B: ReadOnlyBlockDevice>(&mut self, disk: &mut B) -> Result<(), Error> {
        self.class.run(disk, &mut RadioReadPolicy).await
    }
}
