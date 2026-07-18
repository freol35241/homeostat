//! The fake adapter: a minimal unit obeying the supervision contract, used
//! by the fixture house and the supervision integration tests.
//!
//! Contract behavior: connects to HOMEOSTAT_BUS, declares its liveliness
//! token at `home/health/{unit}/alive`, publishes a heartbeat counter as
//! state, and exits cleanly on SIGTERM. Test hooks: `--crash-after-ms` exits
//! nonzero after a delay (0 = before even touching the bus, for crash-loop
//! scenarios), and any publication on the crash key exits nonzero on demand.
//! `--print-stdout`/`--print-stderr` print a known number of lines
//! ("stdout-line-{i}" / "stderr-line-{i}") before touching the bus, for the
//! supervisor's log-capture tests.

use std::env;
use std::time::Duration;

use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};

use homeostat::bus;

#[derive(Parser)]
struct Args {
    /// Milliseconds between heartbeat state publications.
    #[arg(long, default_value_t = 100)]
    heartbeat_ms: u64,
    /// Exit with code 1 after this many milliseconds (0: immediately).
    #[arg(long)]
    crash_after_ms: Option<u64>,
    /// Key to publish the heartbeat counter on.
    #[arg(long, default_value = "home/state/testroom/fake_sensor/value")]
    state_key: String,
    /// Publishing anything on this key makes the adapter exit with code 1.
    #[arg(long, default_value = "home/cmd/testroom/fake_sensor/crash")]
    crash_key: String,
    /// Print this many lines to stdout before touching the bus.
    #[arg(long, default_value_t = 0)]
    print_stdout: u32,
    /// Print this many lines to stderr before touching the bus.
    #[arg(long, default_value_t = 0)]
    print_stderr: u32,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() {
    let args = Args::parse();
    for i in 0..args.print_stdout {
        println!("stdout-line-{i}");
    }
    for i in 0..args.print_stderr {
        eprintln!("stderr-line-{i}");
    }
    if args.crash_after_ms == Some(0) {
        std::process::exit(1);
    }

    let unit = env::var(bus::ENV_UNIT).unwrap_or_else(|_| "fake".to_string());
    let endpoint = env::var(bus::ENV_BUS).expect("HOMEOSTAT_BUS must be set");

    let session = zenoh::open(bus::connect_config(&endpoint))
        .await
        .expect("bus session");
    let _token = session
        .liveliness()
        .declare_token(bus::liveliness_key(&unit))
        .await
        .expect("liveliness token");
    let crash_sub = session
        .declare_subscriber(&args.crash_key)
        .await
        .expect("crash subscriber");
    let publisher = session
        .declare_publisher(&args.state_key)
        .await
        .expect("state publisher");

    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut heartbeat = tokio::time::interval(Duration::from_millis(args.heartbeat_ms));
    let crash_deadline = tokio::time::sleep(Duration::from_millis(
        args.crash_after_ms.unwrap_or(u64::MAX),
    ));
    tokio::pin!(crash_deadline);

    let mut counter: u64 = 0;
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let _ = publisher.put(counter.to_string()).await;
                counter += 1;
            }
            _ = crash_sub.recv_async() => std::process::exit(1),
            _ = &mut crash_deadline, if args.crash_after_ms.is_some() => std::process::exit(1),
            _ = term.recv() => break,
        }
    }
    // Graceful shutdown: token undeclared, session closed, exit 0.
    drop(_token);
    let _ = session.close().await;
}
