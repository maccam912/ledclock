//! This example shows how to use RTC (Real Time Clock) in the RP2040 chip.

#![no_std]
#![no_main]

use core::str::from_utf8;

use chrono::{DateTime as ChronoDateTime, Datelike, Timelike};
use embassy_executor::Spawner;
use embassy_net::{
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use embassy_rp::{
    bind_interrupts,
    clocks::{self, RoscRng},
    dma::{AnyChannel, Channel as DmaChannel},
    into_ref,
    peripherals::{DMA_CH1, PIO1},
    pio::{
        Common, Config, FifoJoin, Instance, InterruptHandler, Pio, PioPin, ShiftConfig,
        ShiftDirection, StateMachine,
    },
    rtc::{DayOfWeek, Rtc},
    Peripheral, PeripheralRef,
};
use embassy_sync::{
    blocking_mutex::raw::ThreadModeRawMutex,
    channel::{Channel, Receiver, Sender},
};
use embassy_time::{Duration, Timer};
use fixed::types::U24F8;
use fixed_macro::fixed;
use log::{error, info};
use rand::{Rng, RngCore};
use reqwless::{
    client::{HttpClient, TlsConfig, TlsVerify},
    request::Method,
};
use serde::Deserialize;
use smart_leds::RGB8;

use {defmt_rtt as _, panic_probe as _};

mod usblogger;
mod wifi;

const WIFI_NETWORK: &str = "Bill Wi the Science Fi";
const WIFI_PASSWORD: &str = "sciencerules";
const NUM_LEDS: usize = 144;
const GLOBAL_BRIGHTNESS: f32 = 0.5;
static CHANNEL: Channel<ThreadModeRawMutex, f32, 4> = Channel::new();

// Define a new channel for sending arrays of u8s
static TWINKLE_CHANNEL: Channel<ThreadModeRawMutex, [u8; NUM_LEDS], 4> = Channel::new();

bind_interrupts!(struct Irqs {
    PIO1_IRQ_0 => InterruptHandler<PIO1>;
});

pub struct Ws2812<'d, P: Instance, const S: usize, const N: usize> {
    dma: PeripheralRef<'d, AnyChannel>,
    sm: StateMachine<'d, P, S>,
}

impl<'d, P: Instance, const S: usize, const N: usize> Ws2812<'d, P, S, N> {
    pub fn new(
        pio: &mut Common<'d, P>,
        mut sm: StateMachine<'d, P, S>,
        dma: impl Peripheral<P = impl DmaChannel> + 'd,
        pin: impl PioPin,
    ) -> Self {
        into_ref!(dma);

        // Setup sm0

        // prepare the PIO program
        let side_set = pio::SideSet::new(false, 1, false);
        let mut a: pio::Assembler<32> = pio::Assembler::new_with_side_set(side_set);

        const T1: u8 = 2; // start bit
        const T2: u8 = 5; // data bit
        const T3: u8 = 3; // stop bit
        const CYCLES_PER_BIT: u32 = (T1 + T2 + T3) as u32;

        let mut wrap_target = a.label();
        let mut wrap_source = a.label();
        let mut do_zero = a.label();
        a.set_with_side_set(pio::SetDestination::PINDIRS, 1, 0);
        a.bind(&mut wrap_target);
        // Do stop bit
        a.out_with_delay_and_side_set(pio::OutDestination::X, 1, T3 - 1, 0);
        // Do start bit
        a.jmp_with_delay_and_side_set(pio::JmpCondition::XIsZero, &mut do_zero, T1 - 1, 1);
        // Do data bit = 1
        a.jmp_with_delay_and_side_set(pio::JmpCondition::Always, &mut wrap_target, T2 - 1, 1);
        a.bind(&mut do_zero);
        // Do data bit = 0
        a.nop_with_delay_and_side_set(T2 - 1, 0);
        a.bind(&mut wrap_source);

        let prg = a.assemble_with_wrap(wrap_source, wrap_target);
        let mut cfg = Config::default();

        // Pin config
        let out_pin = pio.make_pio_pin(pin);
        cfg.set_out_pins(&[&out_pin]);
        cfg.set_set_pins(&[&out_pin]);

        cfg.use_program(&pio.load_program(&prg), &[&out_pin]);

        // Clock config, measured in kHz to avoid overflows
        // TODO CLOCK_FREQ should come from embassy_rp
        let clock_freq = U24F8::from_num(clocks::clk_sys_freq() / 1000);
        let ws2812_freq = fixed!(800: U24F8);
        let bit_freq = ws2812_freq * CYCLES_PER_BIT;
        cfg.clock_divider = clock_freq / bit_freq;

        // FIFO config
        cfg.fifo_join = FifoJoin::TxOnly;
        cfg.shift_out = ShiftConfig {
            auto_fill: true,
            threshold: 24,
            direction: ShiftDirection::Left,
        };

        sm.set_config(&cfg);
        sm.set_enable(true);

        Self {
            dma: dma.map_into(),
            sm,
        }
    }

    pub async fn write(&mut self, colors: &[RGB8; N]) {
        // Precompute the word bytes from the colors
        let mut words = [0u32; N];
        for i in 0..N {
            let word = (u32::from(colors[i].g) << 24)
                | (u32::from(colors[i].r) << 16)
                | (u32::from(colors[i].b) << 8);
            words[i] = word;
        }

        // DMA transfer
        self.sm.tx().dma_push(self.dma.reborrow(), &words).await;

        Timer::after_micros(55).await;
    }
}

#[embassy_executor::task]
async fn star_twinkle(sender: Sender<'static, ThreadModeRawMutex, [u8; NUM_LEDS], 4>) {
    // stars twinkling always increment, but when sending the value via the sender numbers over 128 go back toward zero
    // for example, if state has a 129, a 127 is sent instead. If 255 is sent, a 1 is sent instead.

    let mut rng = RoscRng;
    let mut state = [0u8; NUM_LEDS];

    let star_probability = 0.1;

    for i in 0..NUM_LEDS {
        if rng.gen_bool(star_probability) {
            // This star will be lit. Pick a random value 1 to 255 to set it to
            let brightness = rng.gen_range(1..=255);
            state[i] = brightness;
        }
    }

    // Initialization done

    loop {
        let mut twinkle_data = [0u8; NUM_LEDS];
        for i in 0..NUM_LEDS {
            if i > 36 && i < 72 {
                state[i] = 0;
                continue;
            }
            if state[i] == 0 {
                // This star is not lit
                continue;
            }
            if state[i] == 255 {
                state[i] = 0;
                twinkle_data[i] = 0;
                continue;
            } else if state[i] > 127 {
                twinkle_data[i] = 255 - state[i];
            } else {
                twinkle_data[i] = state[i];
            }
            twinkle_data[i] = twinkle_data[i].clamp(0, 16);
        }

        sender.send(twinkle_data).await;
        Timer::after(Duration::from_millis(10)).await;

        // Increment the state for each star
        for i in 0..NUM_LEDS {
            if state[i] == 0 {
                // This star is not lit
                if rng.gen_bool(0.005) {
                    state[i] = 1;
                }
                continue;
            }
            if state[i] == 255 {
                state[i] = 0;
            }
            state[i] += 1;
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let fw = include_bytes!("cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("cyw43-firmware/43439A0_clm.bin");

    setup_logger!(spawner, p);
    let stack = setup_wifi!(spawner, p, fw, clm, WIFI_NETWORK, WIFI_PASSWORD);

    let mut rng = RoscRng;
    let seed = rng.next_u64();
    let mut rtc = Rtc::new(p.RTC);

    spawner
        .spawn(ws2812_task(
            p.PIO1,
            p.DMA_CH1,
            p.PIN_4,
            CHANNEL.receiver(),
            TWINKLE_CHANNEL.receiver(),
        ))
        .unwrap();

    // Spawn the star_twinkle task
    spawner
        .spawn(star_twinkle(TWINKLE_CHANNEL.sender()))
        .unwrap();

    loop {
        let mut rx_buffer = [0; 8192];
        let mut tls_read_buffer = [0; 16640];
        let mut tls_write_buffer = [0; 16640];

        let client_state = TcpClientState::<1, 1024, 1024>::new();
        let tcp_client = TcpClient::new(stack, &client_state);
        let dns_client = DnsSocket::new(stack);
        let tls_config = TlsConfig::new(
            seed,
            &mut tls_read_buffer,
            &mut tls_write_buffer,
            TlsVerify::None,
        );

        let mut http_client = HttpClient::new_with_tls(&tcp_client, &dns_client, tls_config);
        let url = "https://worldtimeapi.org/api/timezone/America/Chicago";

        info!("connecting to {}", &url);

        let mut request = match http_client.request(Method::GET, &url).await {
            Ok(req) => req,
            Err(e) => {
                error!("Failed to make HTTP request: {:?}", e);
                return; // handle the error
            }
        };

        let response = match request.send(&mut rx_buffer).await {
            Ok(resp) => resp,
            Err(_e) => {
                error!("Failed to send HTTP request");
                return; // handle the error;
            }
        };

        let body = match from_utf8(response.body().read_to_end().await.unwrap()) {
            Ok(b) => b,
            Err(_e) => {
                error!("Failed to read response body");
                return; // handle the error
            }
        };
        info!("Response body: {:?}", &body);

        // parse the response body and update the RTC

        #[derive(Deserialize)]
        struct ApiResponse<'a> {
            datetime: &'a str,
            // other fields as needed
        }

        let bytes = body.as_bytes();
        match serde_json_core::de::from_slice::<ApiResponse>(bytes) {
            Ok((output, _used)) => {
                info!("Datetime: {:?}", output.datetime);

                // Parse the datetime string
                let datetime_str = output.datetime;
                let datetime = ChronoDateTime::parse_from_rfc3339(datetime_str).unwrap();

                let day_of_week_num = datetime.weekday().num_days_from_sunday() as u8;
                let day_of_week = match day_of_week_num {
                    0 => DayOfWeek::Sunday,
                    1 => DayOfWeek::Monday,
                    2 => DayOfWeek::Tuesday,
                    3 => DayOfWeek::Wednesday,
                    4 => DayOfWeek::Thursday,
                    5 => DayOfWeek::Friday,
                    6 => DayOfWeek::Saturday,
                    _ => return, // handle the error
                };
                let rtc_datetime = embassy_rp::rtc::DateTime {
                    year: datetime.year() as u16,
                    month: datetime.month() as u8,
                    day: datetime.day() as u8,
                    day_of_week,
                    hour: datetime.hour() as u8,
                    minute: datetime.minute() as u8,
                    second: datetime.second() as u8,
                };

                info!("RTC updated to: {:?}", &rtc_datetime);
                rtc.set_datetime(rtc_datetime).unwrap();
            }
            Err(_e) => {
                error!("Failed to parse response body");
                return; // handle the error
            }
        }

        for _ in 0..360000 {
            let current_time = rtc.now().unwrap();
            // info!(
            //     "Current RTC time: {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            //     current_time.year,
            //     current_time.month,
            //     current_time.day,
            //     current_time.hour,
            //     current_time.minute,
            //     current_time.second
            // );
            let time_fraction = calculate_time_fraction(current_time);
            CHANNEL.send(time_fraction).await;
            Timer::after(Duration::from_millis(10)).await;
        }
    }
}

fn calculate_time_fraction(current_time: embassy_rp::rtc::DateTime) -> f32 {
    const SECONDS_IN_A_DAY: u32 = 86400; // 24 * 60 * 60
    const TIME_SCALE: u32 = 1;

    let seconds_since_midnight = (current_time.hour as u32 * 3600)
        + (current_time.minute as u32 * 60)
        + (current_time.second as u32);
    let fraction_of_day = seconds_since_midnight as f32 / SECONDS_IN_A_DAY as f32;

    (fraction_of_day * TIME_SCALE as f32) % 1.0
}

#[embassy_executor::task]
async fn ws2812_task(
    pio: PIO1,
    dma: DMA_CH1,
    pin: impl PioPin,
    time_scale_receiver: Receiver<'static, ThreadModeRawMutex, f32, 4>,
    twinkle_receiver: Receiver<'static, ThreadModeRawMutex, [u8; NUM_LEDS], 4>,
) {
    let Pio {
        mut common, sm0, ..
    } = Pio::new(pio, Irqs);
    let mut data = [RGB8::default(); NUM_LEDS];
    let mut ws2812 = Ws2812::new(&mut common, sm0, dma, pin);
    loop {
        let time_scale = time_scale_receiver.receive().await;
        let led_index = (time_scale * NUM_LEDS as f32) % NUM_LEDS as f32;
        let lower_led = (led_index as usize) % NUM_LEDS; // Use integer cast instead of floor
        let upper_led = (lower_led + 1) % NUM_LEDS;
        let fraction = led_index - (lower_led as f32); // Calculate the fractional part

        // Reset all LEDs
        // to imitate sky the leds 36 through 108 will be blue, with 36 up to 72 gradually getting lighter blue, then symmetrically darker from 73 to 108.
        // everything from 0 to 35 will be black, and 109 to 143 will be black.
        for (i, led) in data.iter_mut().enumerate() {
            if i < 36 || i > 108 {
                *led = RGB8::default();
            } else {
                let blue = if i < 72 {
                    ((i - 36) as f32 / 36.0 * 16.0) as u8
                } else {
                    ((108 - i) as f32 / 36.0 * 16.0) as u8
                };

                *led = RGB8 {
                    r: 0,
                    g: 0,
                    b: blue,
                };
            }
        }

        // Set brightness for lower LED
        let lower_brightness = (GLOBAL_BRIGHTNESS * 255.0 * (1.0 - fraction)) as u8;
        data[lower_led] = RGB8 {
            r: lower_brightness,
            g: lower_brightness,
            b: 0,
        };

        // Set brightness for upper LED
        let upper_brightness = (GLOBAL_BRIGHTNESS * 255.0 * fraction) as u8;
        data[upper_led] = RGB8 {
            r: upper_brightness,
            g: upper_brightness,
            b: 0,
        };

        let twinkle_values = twinkle_receiver.receive().await;
        for i in 0..NUM_LEDS {
            if twinkle_values[i] > 0 && data[i].r == 0 {
                data[i] = RGB8 {
                    r: twinkle_values[i] as u8,
                    g: twinkle_values[i] as u8,
                    b: twinkle_values[i] as u8,
                };
            }
        }

        // Write the LED data
        ws2812.write(&data).await;
    }
}
