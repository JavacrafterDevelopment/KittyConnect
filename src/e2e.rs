//! End-to-end encryption check.
//!
//! Two KittyConnects in one process, a host and a friend, connect to each
//! other through real tunnels: the official TailCat for the Hub, File Share,
//! Port Forward and Mounts, and the VOIP helper for a room (voice and video
//! over UDP, chat and attachments over TCP). Everything goes through the
//! app's own code for each tab, the same calls the page's buttons make, and
//! carries random markers. `tools/encryption-check/run.sh` captures every
//! packet on the machine meanwhile, and checks that no marker (and no code)
//! ever crossed the network anywhere but between KittyConnect and its own
//! TailCat on this PC.
//!
//! Linux only, and only when that script runs it (it's ignored otherwise):
//! it needs TailCat programs built for Linux, a display (Xvfb) for the event
//! loop, and root for the capture.

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Sender as Tx};
use std::sync::Arc;

use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::unix::EventLoopBuilderExtUnix;

use super::*;

struct Apps {
    host: App,
    friend: App,
    /// What the friend's room handed the page (video frames).
    friend_page: Vec<Value>,
}

type Cmd = Box<dyn FnOnce(&mut Apps) + Send>;

/// Rooms and hubs tell their events apart by generation; the two apps' start
/// far apart.
const HOST_GEN: u64 = 1_000;
const FRIEND_GEN: u64 = 1_000_000;

/// Each event goes to the app it's for, as main()'s loop would hand it.
fn route(a: &mut Apps, ev: UserEvent) {
    match ev {
        UserEvent::Tc(role, id, ev) => {
            let app = match role {
                Role::ShareHost | Role::PortHost | Role::VoipHost | Role::HubHost => &mut a.host,
                Role::RecvForward | Role::PortJoin | Role::VoipJoin | Role::HubJoin => &mut a.friend,
            };
            app.on_tailcat(role, id, ev);
        }
        UserEvent::Share(e) => {
            a.host.on_share(e);
        }
        UserEvent::Recv(e) => {
            a.friend.on_recv(e);
        }
        UserEvent::Tick => {
            a.host.on_tick();
            a.friend.on_tick();
        }
        UserEvent::Voip(gen, e) => {
            if gen >= FRIEND_GEN {
                if let Some(v) = a.friend.voip.on_event(gen, e) {
                    if v["type"] == "video" {
                        a.friend_page.push(v);
                    }
                }
            } else {
                a.host.voip.on_event(gen, e);
            }
        }
        UserEvent::Hub(gen, e) => {
            let app = if gen >= FRIEND_GEN { &mut a.friend } else { &mut a.host };
            app.hub.on_event(gen, e);
        }
        UserEvent::Ping(t, r) => {
            a.friend.on_ping(t, r);
        }
        UserEvent::Heard { share, heard } => {
            a.host.on_heard(share, heard);
        }
        UserEvent::Lan(k, r) => {
            a.friend.on_lan(k, r);
        }
        UserEvent::Probed(f) => {
            a.host.on_probed(f);
        }
        UserEvent::MountInfo(k, r) => {
            a.friend.on_mount_info(k, r);
        }
        UserEvent::MountDone(k, r) => {
            a.friend.on_mount_done(k, r);
        }
        UserEvent::MountList(k, p, r) => {
            a.friend.on_mount_list(k, p, r);
        }
        UserEvent::MountXfer(k, i, x) => {
            a.friend.on_mount_xfer(k, i, x);
        }
        _ => {}
    }
}

/// The script's handle on the apps, which live on the event loop's thread.
#[derive(Clone)]
struct Drive {
    tx: Tx<Cmd>,
    proxy: EventLoopProxy<UserEvent>,
}

impl Drive {
    /// Runs `f` on the apps and returns what it returns.
    fn on<T: Send + 'static>(&self, f: impl FnOnce(&mut Apps) -> T + Send + 'static) -> T {
        let (rtx, rrx) = channel();
        self.tx
            .send(Box::new(move |a: &mut Apps| {
                let _ = rtx.send(f(a));
            }))
            .expect("event loop gone");
        // Wakes the loop; it runs queued commands after every event.
        let _ = self.proxy.send_event(UserEvent::WebdavLimit(None));
        rrx.recv_timeout(Duration::from_secs(60)).expect("event loop stuck")
    }

    /// Waits until `cond` holds, or fails saying what didn't happen.
    fn wait(&self, what: &str, secs: u64, cond: impl Fn(&mut Apps) -> bool + Send + Sync + 'static) -> Result<(), String> {
        let cond = Arc::new(cond);
        let until = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < until {
            let c = cond.clone();
            if self.on(move |a| c(a)) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(format!("timed out waiting for {what}"))
    }
}

/// A random marker: easy to find in a capture, impossible to come by otherwise.
fn mark(tag: &str) -> String {
    format!("KCMARK{tag}{}", &peers::secret()[..16])
}

#[test]
#[ignore = "run by tools/encryption-check/run.sh"]
fn everything_between_two_kittyconnects_goes_through_the_tunnels() {
    let out = std::env::var("KC_E2E_OUT").expect("run by tools/encryption-check/run.sh");
    let mut el = EventLoopBuilder::<UserEvent>::with_user_event().with_any_thread(true).build();
    let proxy = el.create_proxy();
    let (tx, rx) = channel::<Cmd>();
    let mut apps = Apps { host: App::new(proxy.clone()), friend: App::new(proxy.clone()), friend_page: Vec::new() };
    let work = std::env::temp_dir().join(format!("kc-e2e-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&work);
    apps.friend.recv.inbox = work.join("inbox");

    // The app's 1-second tick.
    let done = Arc::new(AtomicBool::new(false));
    let (d, p) = (done.clone(), proxy.clone());
    std::thread::spawn(move || {
        while !d.load(Ordering::Relaxed) && p.send_event(UserEvent::Tick).is_ok() {
            std::thread::sleep(Duration::from_secs(1));
        }
    });

    let drive = Drive { tx, proxy: proxy.clone() };
    let (d, p, w) = (done.clone(), proxy.clone(), work.clone());
    let script = std::thread::spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| script(&drive, &w)));
        d.store(true, Ordering::Relaxed);
        let _ = p.send_event(UserEvent::WebdavLimit(None));
        match r {
            Ok(r) => r,
            Err(_) => Err("the script panicked".into()),
        }
    });

    el.run_return(|event, _, flow| {
        *flow = ControlFlow::Wait;
        if let Event::UserEvent(ev) = event {
            route(&mut apps, ev);
        }
        while let Ok(cmd) = rx.try_recv() {
            cmd(&mut apps);
        }
        if done.load(Ordering::Relaxed) {
            *flow = ControlFlow::Exit;
        }
    });
    apps.host.shutdown();
    apps.friend.shutdown();
    let report = script.join().unwrap();
    let report = match report {
        Ok(r) => r,
        Err(e) => panic!("{e}"),
    };
    std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    println!("{}", serde_json::to_string_pretty(&report["results"]).unwrap());
    let _ = std::fs::remove_dir_all(&work);
}

fn script(d: &Drive, work: &std::path::Path) -> Result<Value, String> {
    let mut markers = serde_json::Map::new();
    let mut m = |tag: &str| {
        let v = mark(tag);
        markers.insert(tag.to_string(), json!(v));
        v
    };
    let (host_name, friend_name) = (m("NameHost"), m("NameFriend"));
    let room = m("Room");
    let (chat_f, chat_h) = (m("ChatFriend"), m("ChatHost"));
    let (voice_f, voice_h) = (m("VoiceFriend"), m("VoiceHost"));
    let video = m("Video");
    let attach = m("Attach");
    let (share_name, share_body) = (m("ShareName"), m("ShareBody"));
    let port_data = m("Port");
    let (mount_name, mount_body) = (m("MountName"), m("MountBody"));
    let mut results = serde_json::Map::new();
    let mut codes = serde_json::Map::new();

    let (h, f) = (host_name.clone(), friend_name.clone());
    d.on(move |a| {
        a.host.set_name(&h);
        a.friend.set_name(&f);
        a.host.voip.test_gen_base(HOST_GEN);
        a.host.hub.test_gen_base(HOST_GEN);
        a.friend.voip.test_gen_base(FRIEND_GEN);
        a.friend.hub.test_gen_base(FRIEND_GEN);
    });

    // ---------------------------------------------------------------- hub
    d.on(|a| {
        let name = a.host.name.clone();
        a.host.hub.host(&name, &a.host.proxy.clone());
    });
    d.wait("the hub to open", 60, |a| a.host.hub.json()["st"] == "live")?;
    let hub_code = d.on(|a| a.host.hub.json()["code"].as_str().unwrap_or_default().to_string());
    codes.insert("hub".into(), json!(hub_code));
    let hc = hub_code.clone();
    d.on(move |a| {
        let name = a.friend.name.clone();
        a.friend.hub.join(&hc, &name, &a.friend.proxy.clone());
    });
    d.wait("the friend to join the hub", 60, |a| {
        a.friend.hub.json()["st"] == "live" && a.host.hub.json()["members"].as_array().is_some_and(|m| m.len() == 2)
    })?;
    results.insert("hub".into(), json!("friend joined; both see 2 members"));

    // --------------------------------------------------------------- VOIP
    let r = room.clone();
    d.on(move |a| {
        let name = a.host.name.clone();
        a.host.voip.host(&r, &name, &a.host.proxy.clone());
    });
    d.wait("the room to open", 60, |a| a.host.voip.json()["st"] == "live" && a.host.voip.json()["code"].is_string())?;
    let room_code = d.on(|a| a.host.voip.json()["code"].as_str().unwrap_or_default().to_string());
    codes.insert("room".into(), json!(room_code));

    // The room's code goes to the friend through the hub, and they join from the invite.
    let rc = room_code.clone();
    d.on(move |a| a.host.hub.invite(&rc, "Room", Vec::new()))?;
    let rc = room_code.clone();
    d.wait("the hub invite to arrive", 30, move |a| {
        a.friend.hub.json()["invites"].as_array().is_some_and(|l| l.iter().any(|i| i["code"] == rc.as_str()))
    })?;
    d.on(|a| {
        let id = a.friend.hub.json()["invites"][0]["id"].as_u64().unwrap_or(0);
        if let Some(acc) = a.friend.hub.accept(id) {
            a.friend.join_invite(&acc);
        }
    });
    d.wait("the friend to join the room", 90, |a| {
        a.friend.voip.json()["st"] == "live" && a.host.voip.json()["members"].as_array().is_some_and(|m| m.len() == 2)
    })?;
    results.insert("room_via_hub_invite".into(), json!("friend joined the room from the hub invite"));

    // Chat, both ways.
    let (cf, ch) = (chat_f.clone(), chat_h.clone());
    d.on(move |a| {
        a.friend.voip.chat(&cf);
        a.host.voip.chat(&ch);
    });
    let (cf, ch) = (chat_f.clone(), chat_h.clone());
    d.wait("chat both ways", 30, move |a| {
        let has = |v: Value, t: &str| v["chat"].as_array().is_some_and(|l| l.iter().any(|m| m["text"] == t));
        has(a.host.voip.json(), &cf) && has(a.friend.voip.json(), &ch)
    })?;
    results.insert("chat".into(), json!("delivered both ways"));

    // Voice: both join voice, then send packets the way the audio engine does.
    d.on(|a| {
        a.host.voip.test_send(json!({ "t": "voice", "on": true }));
        a.friend.voip.test_send(json!({ "t": "voice", "on": true }));
    });
    d.wait("the room to say both are in voice", 20, |a| {
        a.host.voip.json()["members"].as_array().is_some_and(|m| m.len() == 2 && m.iter().all(|x| x["voice"] == true))
    })?;
    std::thread::sleep(Duration::from_secs(2));
    for seq in 0..100u32 {
        let (vf, vh) = (voice_f.clone(), voice_h.clone());
        d.on(move |a| {
            a.friend.voip.test_voice(seq, vf.as_bytes());
            a.host.voip.test_voice(seq, vh.as_bytes());
        });
        std::thread::sleep(Duration::from_millis(20));
    }
    results.insert("voice".into(), json!("100 packets each way over UDP (checked in the capture)"));

    // Screen share: the host goes live, the friend watches.
    d.on(|a| {
        a.host.voip.screen(Some(json!({ "w": 640, "h": 360, "fps": 30, "codec": "avc1.42E01F", "bitrate": 1_000_000 })));
        let host_id = a.host.voip.json()["me"].as_u64().unwrap_or(0) as u16;
        a.friend.voip.watch(Some(host_id));
    });
    std::thread::sleep(Duration::from_secs(2));
    // A frame several pieces long, the marker all through it.
    let frame: Vec<u8> = video.as_bytes().iter().copied().cycle().take(6000).collect();
    for i in 0..30u64 {
        let fr = frame.clone();
        d.on(move |a| {
            a.host.voip.video(true, 0, i * 33_000, &fr);
        });
        std::thread::sleep(Duration::from_millis(100));
    }
    let vm = video.clone();
    d.wait("the friend to receive video", 30, move |a| {
        use base64::Engine as _;
        a.friend_page.iter().any(|v| {
            base64::engine::general_purpose::STANDARD
                .decode(v["data"].as_str().unwrap_or(""))
                .is_ok_and(|b| String::from_utf8_lossy(&b).contains(&vm))
        })
    })?;
    let frames = d.on(|a| a.friend_page.len());
    results.insert("video".into(), json!(format!("friend received {frames} frames over UDP")));

    // An attachment in chat, from the friend.
    let attach_file = work.join(format!("{attach}.txt"));
    std::fs::write(&attach_file, attach.repeat(4000)).map_err(|e| e.to_string())?;
    let af = attach_file.clone();
    d.on(move |a| {
        let p = a.friend.proxy.clone();
        a.friend.voip.attach(vec![af], String::new(), &p);
    });
    let an = format!("{attach}.txt");
    d.wait("the attachment to reach the room", 60, move |a| {
        a.host.voip.json()["chat"].as_array().is_some_and(|l| l.iter().any(|m| m["file"]["name"] == an.as_str()))
    })?;
    results.insert("attachment".into(), json!("uploaded through the tunnel"));

    // ---------------------------------------------------------- file share
    let share_file = work.join(&share_name);
    let body = share_body.repeat(20_000);
    std::fs::write(&share_file, &body).map_err(|e| e.to_string())?;
    let sf = share_file.clone();
    d.on(move |a| {
        share::add_paths(&a.host.share.lib, &[sf]);
        a.host.start_share();
    });
    d.wait("the file share to open", 60, |a| a.host.share.st == St::Live && a.host.share.addr.is_some())?;
    let share_code = d.on(|a| code::file(a.host.share.server.as_ref().map(|s| s.port).unwrap_or(0), a.host.share.addr.as_deref().unwrap_or("")));
    codes.insert("share".into(), json!(share_code));
    let sc = share_code.clone();
    d.on(move |a| a.friend.start_recv(&sc));
    d.wait("the file list", 60, |a| !a.friend.recv.files.is_empty())?;
    d.on(|a| {
        let ids: Vec<u64> = a.friend.recv.files.iter().map(|f| f.id).collect();
        a.friend.queue(&ids);
    });
    d.wait("the download", 60, |a| a.friend.recv.states.values().any(|s| s.st == "done"))?;
    let got = d.on(|a| a.friend.recv.states.values().find_map(|s| s.path.clone()));
    let got = got.ok_or("no downloaded file")?;
    if std::fs::read_to_string(&got).map_err(|e| e.to_string())? != body {
        return Err("the downloaded file differs".into());
    }
    results.insert("file_share".into(), json!(format!("{} bytes downloaded, identical", body.len())));

    // -------------------------------------------------------- port forward
    let echo = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
    let echo_port = echo.local_addr().map_err(|e| e.to_string())?.port();
    std::thread::spawn(move || {
        for c in echo.incoming().flatten() {
            std::thread::spawn(move || {
                let mut c = c;
                let mut buf = [0u8; 4096];
                while let Ok(n) = std::io::Read::read(&mut c, &mut buf) {
                    if n == 0 || std::io::Write::write_all(&mut c, &buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    d.on(move |a| a.host.start_port_host(&echo_port.to_string(), None));
    d.wait("the port to open", 60, move |a| a.host.hosts.iter().any(|h| h.ports == [echo_port] && h.addr.is_some()))?;
    let port_code =
        d.on(move |a| a.host.hosts.iter().find(|h| h.ports == [echo_port]).and_then(|h| h.addr.clone()).unwrap_or_default());
    codes.insert("port".into(), json!(port_code));
    let local = free_port()?;
    let pc = port_code.clone();
    d.on(move |a| a.friend.start_port_join(&pc, &echo_port.to_string(), &[local], false, JoinFrom::Manual));
    d.wait("the port join to pass its port check", 60, move |a| {
        a.friend.joins.iter().any(|j| j.maps == [(local, echo_port)] && j.st == St::Live)
    })?;
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, local)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(15))).ok();
    let payload = port_data.repeat(500);
    std::io::Write::write_all(&mut s, payload.as_bytes()).map_err(|e| e.to_string())?;
    let mut back = vec![0u8; payload.len()];
    std::io::Read::read_exact(&mut s, &mut back).map_err(|e| e.to_string())?;
    if back != payload.as_bytes() {
        return Err("the port echo differs".into());
    }
    drop(s);
    results.insert("port_forward".into(), json!(format!("{} bytes echoed through the tunnel", payload.len())));

    // The 25566 case: a port that code didn't open.
    let wrong = if echo_port == 65535 { echo_port - 1 } else { echo_port + 1 };
    let wrong_local = free_port()?;
    let pc = port_code.clone();
    d.on(move |a| a.friend.start_port_join(&pc, &wrong.to_string(), &[wrong_local], false, JoinFrom::Manual));
    d.wait("the wrong port to be refused", 60, move |a| {
        a.friend.joins.iter().any(|j| j.maps == [(wrong_local, wrong)] && j.st == St::Error && j.error.contains("isn't open"))
    })?;
    let listening = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, wrong_local)), Duration::from_millis(500)).is_ok();
    if listening {
        return Err("something still listens for the wrong port".into());
    }
    results.insert("wrong_port".into(), json!(format!("port {wrong}: refused by the host's check, nothing left listening")));

    // -------------------------------------------------------------- mounts
    let mount_dir = work.join(&mount_name);
    std::fs::create_dir_all(&mount_dir).map_err(|e| e.to_string())?;
    let mount_file = format!("{mount_body}.txt");
    let mbody = mount_body.repeat(5_000);
    std::fs::write(mount_dir.join(&mount_file), &mbody).map_err(|e| e.to_string())?;
    let md = mount_dir.clone();
    d.on(move |a| {
        a.host.mount_pick = Some(md);
        a.host.start_mount_host(false);
    });
    d.wait("the mount to open", 60, |a| a.host.hosts.iter().any(|h| h.mount.is_some() && h.addr.is_some()))?;
    let mount_code = d.on(|a| {
        a.host.hosts.iter().find(|h| h.mount.is_some()).map(|h| code::mount(h.ports[0], h.addr.as_deref().unwrap_or(""))).unwrap_or_default()
    });
    codes.insert("mount".into(), json!(mount_code));
    let mc = mount_code.clone();
    d.on(move |a| a.friend.start_port_join(&mc, "", &[], false, JoinFrom::Mount));
    d.wait("the mount join and its info", 60, |a| {
        a.friend.joins.iter().any(|j| j.st == St::Live && j.mount.as_ref().is_some_and(|m| m.info.is_some()))
    })?;
    let (mport, mname) = d.on(|a| {
        let j = a.friend.joins.iter().find(|j| j.mount.as_ref().is_some_and(|m| m.info.is_some())).unwrap();
        (j.maps[0].0, j.mount.as_ref().unwrap().info.as_ref().unwrap().name.clone())
    });
    let listed = mount::list(mport, "")?;
    if !listed.iter().any(|e| e.name == mount_file) {
        return Err("the mount's listing is missing the file".into());
    }
    let cancel = AtomicBool::new(false);
    let saved = mount::download(mport, &mname, &mount_file, &work.join("mount-dl"), &cancel, &mut |_, _| {})?;
    if std::fs::read_to_string(&saved).map_err(|e| e.to_string())? != mbody {
        return Err("the mount download differs".into());
    }
    results.insert("mount".into(), json!(format!("listed and downloaded {} bytes", mbody.len())));

    // ---------------------------------------------------- how each went
    let paths = d.on(|a| {
        json!({
            "hub": a.friend.hub.link.path(),
            "room": a.friend.voip.json()["link"]["path"],
            "file_share": a.friend.recv.link.path(),
            "joins": a.friend.joins.iter().filter(|j| j.st == St::Live).map(|j| j.link.path()).collect::<Vec<_>>(),
        })
    });
    results.insert("paths".into(), paths);
    Ok(json!({ "markers": markers, "codes": codes, "results": results }))
}
