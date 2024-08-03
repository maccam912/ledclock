#[macro_export]
macro_rules! setup_wifi {
    ($spawner:expr, $p:expr, $fw:expr, $clm:expr, $ssid:expr, $password:expr) => {
        {
            use defmt::unwrap;
            use embassy_rp::bind_interrupts;
            use embassy_rp::gpio::{Level, Output};
            use embassy_rp::peripherals::{DMA_CH0, PIO0};
            use embassy_rp::pio::{InterruptHandler, Pio};
            use cyw43_pio::PioSpi;
            use embassy_net::{Config, Stack, StackResources};
            use static_cell::StaticCell;
            use embassy_time::Timer;
            use rand::RngCore;
            use embassy_rp::clocks::RoscRng;

            bind_interrupts!(struct Irqs {
                PIO0_IRQ_0 => InterruptHandler<PIO0>;
            });

            #[embassy_executor::task]
            async fn wifi_task(runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>) -> ! {
                runner.run().await
            }

            #[embassy_executor::task]
            async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
                stack.run().await
            }

            let mut rng = RoscRng;
            let pwr = Output::new($p.PIN_23, Level::Low);
            let cs = Output::new($p.PIN_25, Level::High);
            let mut pio = Pio::new($p.PIO0, Irqs);
            let spi = PioSpi::new(&mut pio.common, pio.sm0, pio.irq0, cs, $p.PIN_24, $p.PIN_29, $p.DMA_CH0);

            static STATE: StaticCell<cyw43::State> = StaticCell::new();
            let state = STATE.init(cyw43::State::new());
            let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, $fw).await;
            unwrap!($spawner.spawn(wifi_task(runner)));

            control.init($clm).await;
            control
                .set_power_management(cyw43::PowerManagementMode::PowerSave)
                .await;

            let config = Config::dhcpv4(Default::default());
            let seed = rng.next_u64();

            static STACK: StaticCell<Stack<cyw43::NetDriver<'static>>> = StaticCell::new();
            static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
            let stack = &*STACK.init(Stack::new(
                net_device,
                config,
                RESOURCES.init(StackResources::<5>::new()),
                seed,
            ));

            unwrap!($spawner.spawn(net_task(stack)));

            loop {
                match control.join_wpa2($ssid, $password).await {
                    Ok(_) => break,
                    Err(err) => {
                        info!("join failed with status={}", err.status);
                    }
                }
            }

            info!("waiting for DHCP...");
            while !stack.is_config_up() {
                Timer::after_millis(100).await;
            }
            info!("DHCP is now up!");

            info!("waiting for link up...");
            while !stack.is_link_up() {
                Timer::after_millis(500).await;
            }
            info!("Link is up!");

            info!("waiting for stack to be up...");
            stack.wait_config_up().await;
            info!("Stack is up!");

            stack
        }
    };
}
