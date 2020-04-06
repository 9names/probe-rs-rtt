use probe_rs::{config::TargetSelector, DebugProbeInfo, Probe};
use probe_rs_rtt::{Channels, Rtt, RttChannel};
use std::io::prelude::*;
use std::io::{stdin, stdout};
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use structopt::StructOpt;

#[derive(Debug, PartialEq, Eq)]
enum ProbeInfo {
    Number(usize),
    List,
}

impl std::str::FromStr for ProbeInfo {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<ProbeInfo, &'static str> {
        if s == "list" {
            Ok(ProbeInfo::List)
        } else if let Ok(n) = s.parse::<usize>() {
            Ok(ProbeInfo::Number(n))
        } else {
            Err("Invalid probe number.")
        }
    }
}

#[derive(Debug, StructOpt)]
#[structopt(
    name = "rtthost",
    about = "Host program for debugging microcontrollers using the RTT (real-time transfer) protocol."
)]
struct Opts {
    #[structopt(
        short,
        long,
        default_value = "0",
        help = "Specify probe number or 'list' to list probes."
    )]
    probe: ProbeInfo,

    #[structopt(
        short,
        long,
        help = "Target chip type. Leave unspecified to auto-detect."
    )]
    chip: Option<String>,

    #[structopt(short, long, help = "List RTT channels and exit.")]
    list: bool,

    #[structopt(
        short,
        long,
        help = "Number of up channel to output. Defaults to 0 if it exists."
    )]
    up: Option<usize>,

    #[structopt(
        short,
        long,
        help = "Number of down channel for keyboard input. Defaults to 0 if it exists."
    )]
    down: Option<usize>,
}

fn main() {
    pretty_env_logger::init();

    std::process::exit(run());
}

fn run() -> i32 {
    let opts = Opts::from_args();

    let probes = Probe::list_all();

    if probes.len() == 0 {
        eprintln!("No debug probes available. Make sure your probe is plugged in, supported and up-to-date.");
        return 1;
    }

    let probe_number = match opts.probe {
        ProbeInfo::List => {
            list_probes(std::io::stdout(), &probes);
            return 0;
        }
        ProbeInfo::Number(i) => i,
    };

    if probe_number >= probes.len() {
        eprintln!("Probe {} does not exist.", probe_number);
        list_probes(std::io::stderr(), &probes);
        return 1;
    }

    let probe = match probes[probe_number].open() {
        Ok(probe) => probe,
        Err(err) => {
            eprintln!("Error opening probe: {}", err);
            return 1;
        }
    };

    let target_selector = opts
        .chip
        .clone()
        .map(|t| TargetSelector::Unspecified(t))
        .unwrap_or(TargetSelector::Auto);

    let session = match probe.attach(target_selector) {
        Ok(session) => session,
        Err(err) => {
            eprintln!("Error creating debug session: {}", err);

            if opts.chip.is_none() {
                if let probe_rs::Error::ChipNotFound(_) = err {
                    eprintln!("Hint: Use '--chip' to specify the target chip type manually");
                }
            }

            return 1;
        }
    };

    let core = match session.attach_to_core(0) {
        Ok(core) => core,
        Err(err) => {
            eprintln!("Error attaching to core 0: {}", err);
            return 1;
        }
    };

    eprintln!("Attaching to RTT...");

    let mut rtt = match Rtt::attach(core, &session) {
        Ok(rtt) => rtt,
        Err(err) => {
            eprintln!("Error attaching to RTT: {}", err);
            return 1;
        }
    };

    if opts.list {
        println!("Up channels:");
        list_channels(rtt.up_channels());

        println!("Down channels:");
        list_channels(rtt.down_channels());

        return 0;
    }

    let up_channel = if let Some(up) = opts.up {
        let chan = rtt.up_channels().take(up);

        if chan.is_none() {
            eprintln!("Error: up channel {} does not exist.", up);
            return 1;
        }

        chan
    } else {
        rtt.up_channels().take(0)
    };

    let down_channel = if let Some(down) = opts.down {
        let chan = rtt.down_channels().take(down);

        if chan.is_none() {
            eprintln!("Error: up channel {} does not exist.", down);
            return 1;
        }

        chan
    } else {
        rtt.down_channels().take(0)
    };

    let stdin = down_channel.as_ref().map(|_| stdin_channel());

    eprintln!("Found control block at 0x{:08x}", rtt.ptr());

    let mut up_buf = [0u8; 1024];
    let mut down_buf = vec![];

    loop {
        if let Some(up_channel) = up_channel.as_ref() {
            let count = match up_channel.read(up_buf.as_mut()) {
                Ok(count) => count,
                Err(err) => {
                    eprintln!("\nError reading from RTT: {}", err);
                    return 1;
                }
            };

            match stdout().write_all(&up_buf[..count]) {
                Ok(_) => {
                    stdout().flush().ok();
                }
                Err(err) => {
                    eprintln!("Error writing to stdout: {}", err);
                    return 1;
                }
            }
        }

        if let (Some(down_channel), Some(stdin)) = (down_channel.as_ref(), &stdin) {
            if let Ok(bytes) = stdin.try_recv() {
                down_buf.extend_from_slice(bytes.as_slice());
            }

            if !down_buf.is_empty() {
                let count = match down_channel.write(down_buf.as_mut()) {
                    Ok(count) => count,
                    Err(err) => {
                        eprintln!("\nError writing to RTT: {}", err);
                        return 1;
                    }
                };

                if count > 0 {
                    down_buf.drain(..count);
                }
            }
        }
    }
}

fn list_probes(mut stream: impl std::io::Write, probes: &Vec<DebugProbeInfo>) {
    writeln!(stream, "Available probes:").unwrap();

    for (i, probe) in probes.iter().enumerate() {
        writeln!(
            stream,
            "  {}: {} {}",
            i,
            probe.identifier,
            probe
                .serial_number
                .as_ref()
                .map(|s| &**s)
                .unwrap_or("(no serial number)")
        )
        .unwrap();
    }
}

fn list_channels(channels: &Channels<impl RttChannel>) {
    if channels.is_empty() {
        println!("  (none)");
        return;
    }

    for chan in channels.iter() {
        println!(
            "  {}: {} (buffer size {})",
            chan.number(),
            chan.name().as_ref().map(|s| &**s).unwrap_or("(no name)"),
            chan.buffer_size(),
        );
    }
}

fn stdin_channel() -> Receiver<Vec<u8>> {
    let (tx, rx) = channel();

    thread::spawn(move || {
        let mut buf = [0u8; 1024];

        loop {
            match stdin().read(&mut buf[..]) {
                Ok(count) => {
                    tx.send(buf[..count].to_vec()).unwrap();
                }
                Err(err) => {
                    eprintln!("Error reading from stdin, input disabled: {}", err);
                    break;
                }
            }
        }
    });

    rx
}
