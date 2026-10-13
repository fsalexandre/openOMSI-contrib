//! A load test of a server: `n` players that join it, each with a bus the server has, start
//! at a place on its roads (`x,y`: the server's log names one, "first neighbouring lane pair
//! at (x, y)") and then drive where its AI cars drive (each bot takes one car's place, frame
//! by frame, so they spread over the roads the server simulates). Every 10 s: how many
//! players each bot sees and what the server sends them all (the bytes they receive).
//!   cargo run --release -p omsi-net --example load_bots -- 127.0.0.1:27015 30 600 493655,2979715 "Vehicles/MAN_SD200/MAN_SD80.bus;Vehicles/MAN_SD200/MAN_SD77.bus"
use omsi_net::{LanSession, Pose, WorldInfo};
use std::time::{Duration, Instant};

struct Bot {
    lan: LanSession,
    pose: Pose,
    /// The AI car it drives in place of (none yet: it stands where it started).
    car: Option<u32>,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:27015".into());
    let n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(300);
    let start_at: Vec<f64> = args.next().unwrap_or_default().split(',').filter_map(|v| v.trim().parse().ok()).collect();
    let (x0, y0) = (start_at.first().copied().unwrap_or(0.0), start_at.get(1).copied().unwrap_or(0.0));
    let buses: Vec<String> = args.next().unwrap_or_else(|| "Vehicles/MAN_SD200/MAN_SD80.bus".into()).split(';').map(str::to_string).collect();
    let addrs: Vec<std::net::SocketAddr> = std::net::ToSocketAddrs::to_socket_addrs(&addr).expect("address").collect();
    let mut bots: Vec<Bot> = Vec::new();
    let (start, dt) = (Instant::now(), 1.0 / 30.0);
    let (mut last, mut last_bytes, mut joined) = (Instant::now(), 0u64, Instant::now() - Duration::from_secs(9));
    while start.elapsed() < Duration::from_secs(secs) {
        let step = Instant::now();
        // one more every 2 s (thirty at once would be no players' way of coming)
        if bots.len() < n && joined.elapsed() >= Duration::from_secs(2) {
            let i = bots.len();
            let name = format!("bot{i:02}");
            let lan = LanSession::join_addr(addrs.clone(), None, &name, WorldInfo::default()).expect("join");
            // (a few metres apart, as buses at a stop)
            let pose = Pose { name, bus: buses[i % buses.len()].clone(), length: 12.0, width: 2.5, height: 3.0, x: x0 + 15.0 * i as f64, y: y0, flags: omsi_net::FLAG_VEHICLE, ..Default::default() };
            bots.push(Bot { lan, pose, car: None });
            joined = Instant::now();
            if bots.len() == n {
                println!("{:>4} s: all {n} bots joined {addr}", start.elapsed().as_secs());
            }
        }
        for (i, b) in bots.iter_mut().enumerate() {
            for (_, frame) in b.lan.take_world() {
                let car = match b.car {
                    Some(id) => frame.cars.iter().find(|c| c.id == id),
                    // the i-th car it hears of, so that the bots spread
                    None => frame.cars.get(i % frame.cars.len().max(1)),
                };
                if let Some(c) = car {
                    b.car = Some(c.id);
                    (b.pose.x, b.pose.y, b.pose.z, b.pose.heading, b.pose.speed_kmh) = (c.x, c.y, c.z, c.heading, c.speed * 3.6);
                } else if frame.gone.iter().any(|&(person, id)| !person && Some(id) == b.car) {
                    b.car = None;
                }
            }
            b.lan.tick(dt as f32, &b.pose);
        }
        if last.elapsed() >= Duration::from_secs(10) {
            let bytes: u64 = bots.iter().map(|b| b.lan.received).sum();
            let seen: Vec<usize> = bots.iter().map(|b| b.lan.peers().filter(|p| p.has_info).count()).collect();
            let driving = bots.iter().filter(|b| b.car.is_some()).count();
            println!(
                "{:>4} s: {driving}/{n} driving, they see {}..{} players, the server sends them {:.0} kB/s",
                start.elapsed().as_secs(),
                seen.iter().min().unwrap_or(&0),
                seen.iter().max().unwrap_or(&0),
                (bytes - last_bytes) as f64 / 1024.0 / last.elapsed().as_secs_f64()
            );
            (last, last_bytes) = (Instant::now(), bytes);
        }
        std::thread::sleep(Duration::from_secs_f64(dt).saturating_sub(step.elapsed()));
    }
    for b in &bots {
        b.lan.leave();
    }
}
