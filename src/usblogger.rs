//! This example shows how to use USB (Universal Serial Bus) in the RP2040 chip.
//!
//! This creates the possibility to send log::info/warn/error/debug! to USB serial port.

#[macro_export]
macro_rules! setup_logger {
    ($spawner:expr, $p:expr) => {
        {
            use embassy_rp::bind_interrupts;
            use embassy_rp::peripherals::USB;
            use embassy_rp::usb::{Driver, InterruptHandler};
            use {defmt_rtt as _, panic_probe as _};

            bind_interrupts!(struct Irqs {
                USBCTRL_IRQ => InterruptHandler<USB>;
            });

            #[embassy_executor::task]
            async fn logger_task(driver: Driver<'static, USB>) {
                embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
            }

            let driver = Driver::new($p.USB, Irqs);
            $spawner.spawn(logger_task(driver)).unwrap();
        }
    };
}
