//! The integration suites, in one test binary. Each suite runs real
//! processes: a supervisor on a fixture house, a broker, fake devices. The
//! crate is linked once, and every suite's tests share one pool. A suite
//! is a file here with a `mod` line below; a file without one is never
//! compiled. Run one suite with `cargo test --test it <suite>::`.

mod civil;
mod common;

mod aduro;
mod arbiter;
mod buttons;
mod companion;
mod dashboard;
mod esphome;
mod evening;
mod forecast;
mod go2rtc;
mod history;
mod ivt490;
mod mcp;
mod ntfy;
mod onvif;
mod openwrt;
mod owntracks;
mod persons;
mod plan_apply;
mod recorder;
mod replay;
mod restore;
mod rf433;
mod supervision;
mod templates;
mod virtual_entities;
mod z2m;
