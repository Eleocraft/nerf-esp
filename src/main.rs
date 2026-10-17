#![no_std]
#![no_main]

mod phy;

use core::net::SocketAddr;

use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::ethernet::clock::ExternalRefClock;
use esp_hal::ethernet::{Ethernet, EthernetDmaStorage, RmiiPinBundle};
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::mcpwm::operator::PwmPinConfig;
use esp_hal::mcpwm::timer::PwmWorkingMode;
use esp_hal::mcpwm::{McPwm, PeripheralClockConfig};
use esp_hal::rng::Rng;
use esp_hal::time::Rate;

use esp_hal::timer::timg::TimerGroup;
// defmt logger
use esp_println as _;
// panic handler
use esp_backtrace as _;
use static_cell::{ConstStaticCell, StaticCell};

use crate::phy::AsyncPhy;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

/// MAC address
const MAC_ADDR: [u8; 6] = [0x02, 0x00, 0x00, 0xAB, 0xCD, 0xEF];

/// DMA storage — must outlive `eth`, so we keep it as a `'static`.
static STORAGE: ConstStaticCell<EthernetDmaStorage<10, 10>> =
    ConstStaticCell::new(EthernetDmaStorage::new());

static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();

type EthDevice = Ethernet<'static, esp_hal::Async, AsyncPhy>;

// buffers for tcp data before and after processing
static NATS_TCP_RX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
static NATS_TCP_TX_BUF: StaticCell<[u8; 256]> = StaticCell::new();

// NATS
const NATS_MAX_TOPIC_SIZE: usize = 64;
const NATS_MAX_MESSAGE_SIZE: usize = 256;
type NatsCollections = embassy_nats::Heapless<NATS_MAX_TOPIC_SIZE, NATS_MAX_MESSAGE_SIZE>;
static NATS_STORAGE: embassy_nats::Storage<NatsCollections> = embassy_nats::Storage::new();
const NATS_ADDR: &str = "nats.lan";
const NATS_PORT: u16 = 4222;
const NATS_USER: &str = "nats";
const NATS_PWD: &str = "south";
const NATS_NUM_SUBS: usize = 2;

const NATS_MSG_CHANNEL_SIZE: usize = 3;
static MSG_CH: embassy_nats::MsgChannel<NatsCollections, NATS_MSG_CHANNEL_SIZE> =
    embassy_nats::MsgChannel::new();

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, EthDevice>) {
    runner.run().await
}

#[embassy_executor::task]
async fn nats_task(
    mut runner: embassy_nats::Runner<'static, NatsCollections, embassy_nats::UserPwdAuthenticator, NATS_NUM_SUBS>,
) -> ! {
    runner.run().await
}

async fn resolve_addr(
    stack: &Stack<'_>,
    addr: &str,
    port: u16,
) -> Result<SocketAddr, embassy_net::dns::Error> {
    let ips = stack.dns_query(addr, DnsQueryType::A).await?;
    let Some(ip) = ips.first() else {
        return Err(embassy_net::dns::Error::Failed);
    };
    Ok(SocketAddr::new((*ip).into(), port))
}

#[esp_hal::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let p = esp_hal::init(config);

    // init rtos
    let timg0 = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timg0.timer0, p.FROM_CPU_INTR0);
    
    // ── PHY reset ─────────────────────────────────────────────────────────────
    // Assert reset for at least 100 ms, then release and wait ≥ 300 ms for the PHY to stabilise.
    let mut phy_reset = Output::new(p.GPIO5, Level::Low, OutputConfig::default());

    Timer::after(Duration::from_millis(100)).await;
    phy_reset.set_high();
    Timer::after(Duration::from_millis(300)).await;

    // ── Ethernet ─────────────────────────────────────────────────────────────

    let eth: EthDevice = Ethernet::new(
        p.ETH,
        STORAGE.take(),
        MAC_ADDR,
        AsyncPhy::new_auto(),
        RmiiPinBundle {
            clock: ExternalRefClock::new(p.GPIO0), // REF_CLK from IP101GRI REFCLKO
            rxd0: p.GPIO25,
            rxd1: p.GPIO26,
            rx_dv: p.GPIO27,
            txd0: p.GPIO19,
            txd1: p.GPIO22,
            tx_en: p.GPIO21,
            mdc: p.GPIO23,
            mdio: p.GPIO18,
        },
    )
    .expect("Ethernet init failed")
    .into_async();
    
    // ── Network stack ─────────────────────────────────────────────────────────

    let config = embassy_net::Config::dhcpv4(Default::default());

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        eth,
        config,
        RESOURCES.init(StackResources::<3>::new()),
        seed,
    );

    // launch network task
    spawner.spawn(net_task(runner).unwrap());

    // ── Wait for DHCP ─────────────────────────────────────────────────────────

    info!("Waiting for DHCP lease…");

    stack.wait_config_up().await;

    if let Some(cfg) = stack.config_v4() {
        info!("IP:      {}", cfg.address);
        info!("Gateway: {:?}", cfg.gateway);
    }
    
    // ── Connect to NATS ───────────────────────────────────────────────────────

    let socket = TcpSocket::new(
        stack,
        NATS_TCP_RX_BUF.init([0; _]),
        NATS_TCP_TX_BUF.init([0; _]),
    );

    // resolve nats addr
    let socket_addr = loop {
        match resolve_addr(&stack, NATS_ADDR, NATS_PORT).await {
            Ok(addr) => break addr,
            Err(e) => {
                warn!("could not resolve nats addr: {:?}, retrying...", e);
                Timer::after_secs(2).await;
            }
        }
    };

    // nats connection
    let (mut client, runner) =
        embassy_nats::new_with_user_pwd(NATS_USER, NATS_PWD, socket_addr, socket, &NATS_STORAGE)
            .unwrap();

    // launch nats task
    spawner.spawn(nats_task(runner).unwrap());

    client.subscribe(
        heapless::String::try_from("nerf.trigger").unwrap(),
        &MSG_CH,
        )
        .await
        .unwrap();

    // pwm peripheral
    let pwm_clk = PeripheralClockConfig::with_frequency(Rate::from_mhz(10)).unwrap();
    let mut pwm = McPwm::new(p.MCPWM0, pwm_clk);

    // connect op0 to timer0
    pwm.operator0.set_timer(&pwm.timer0);
    
    // connect op0 to pin
    let mut pwm_pin = pwm
        .operator0
        .with_pin_a(p.GPIO17, PwmPinConfig::UP_ACTIVE_HIGH);

    // config timer with counter values in the range of 0..20000 and 50hz period
    // 1 tick = 1 us
    let timer_clock_cfg = pwm_clk
        .timer_clock_with_frequency(19999, PwmWorkingMode::Increase, Rate::from_hz(50))
        .unwrap();

    // start timer0
    pwm.timer0.start(timer_clock_cfg);
    
    info!("Running");
    loop {
        
        if b"FIRE" != client.receive().await.data.as_slice() {
            continue;
        }

        info!("Fire!");

        pwm_pin.set_timestamp(1000);
        Timer::after_secs(3).await;

        pwm_pin.set_timestamp(1300);
        Timer::after_secs(1).await;
    }
}
