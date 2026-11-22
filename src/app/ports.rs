//! Ports: hosting them, and joining someone else's.

use crate::*;

impl App {
    pub(crate) fn key(&mut self) -> u64 {
        self.next_key += 1;
        self.next_key
    }

    /// Opens another port set; each one is listed with its own code and Stop.
    pub(crate) fn start_port_host(&mut self, input: &str, game: Option<String>) {
        self.host_error.clear();
        let ports = match code::parse_ports(input) {
            Ok(p) => p,
            Err(e) => return self.host_error = e,
        };
        if game.is_some() && ports.len() != 1 {
            return self.host_error = "A game server opens one port.".into();
        }
        // Minecraft: hear this PC's own "Open to LAN" so friends can see the world.
        if game.as_deref() == Some("minecraft") && !lan::listen() {
            self.host_error = "Couldn't listen for Minecraft LAN worlds (UDP 4445). The port still opens; friends can add the server by address.".into();
        }
        if let Some(p) = ports.iter().find(|p| self.hosts.iter().any(|h| h.proc.is_some() && h.ports.contains(p))) {
            return self.host_error = format!("Port {p} is already open below.");
        }
        // Whether a server is running there yet is checked in the background (`probe`).
        let warn = String::new();
        let mut list: Vec<String> = ports.iter().map(|p| p.to_string()).collect();
        // The beacon rides in the same tunnel so the friend can tell we're still here.
        if let Some(b) = beacon::ensure(&self.proxy).filter(|b| !ports.contains(b)) {
            list.push(b.to_string());
        }
        let args = vec!["serve".into(), "--key=new".into(), list.join(",")];
        match tailcat::spawn(Role::PortHost, &args, &self.proxy) {
            Ok(p) => {
                let key = self.key();
                self.hosts.insert(0, PortHost {
                    key,
                    st: St::Starting,
                    error: String::new(),
                    warn,
                    proc: Some(p),
                    ports,
                    addr: None,
                    link: Link::default(),
                    peers: Peers::default(),
                    game,
                    mount: None,
                    probed: false,
                    lan_note: String::new(),
                    log: Log::default(),
                });
                self.set_status("Starting TailCat…", "busy");
            }
            Err(e) => self.host_error = e,
        }
    }

    /// `from` says which form: Game servers and Mounts take codes that carry
    /// their port, and pick a free local port themselves.
    pub(crate) fn start_port_join(&mut self, input: &str, ports_input: &str, locals: &[u16], lan: bool, from: JoinFrom) {
        self.join_error.clear();
        let game_mode = from != JoinFrom::Manual;
        let c = match code::parse(input) {
            Ok(c) if c.kind == Kind::File => {
                return self.join_error = "That's a file share code. Paste it in the File Share tab.".into()
            }
            Ok(c) if c.kind == Kind::Voip => return self.join_error = "That's a room code. Paste it in the VOIP tab.".into(),
            Ok(c) if c.kind == Kind::Hub => return self.join_error = HUB_CODE_HERE.into(),
            Ok(c) if from != JoinFrom::Mount && c.kind == Kind::Mount => {
                return self.join_error = "That's a mount code. Paste it in the Mounts tab.".into()
            }
            Ok(c) if from == JoinFrom::Mount && c.kind != Kind::Mount => {
                return self.join_error = "That isn't a mount code. Ask for the code from the Mounts tab (it starts with kitty:mount:).".into()
            }
            Ok(c) if from == JoinFrom::Game && c.kind != Kind::Game => {
                return self.join_error =
                    "That's a manual port code. Paste it in Manual port forward, or ask for the game code (it starts with kitty:game.).".into()
            }
            Ok(c) => c,
            Err(e) => return self.join_error = e,
        };
        let game = c.game.clone();
        // The port numbers come from the box; an older kitty:port code carries its own.
        let remotes = if ports_input.trim().is_empty() && !c.ports.is_empty() {
            c.ports.clone()
        } else {
            match code::parse_ports(ports_input) {
                Ok(p) => p,
                Err(_) if ports_input.trim().is_empty() => {
                    return self.join_error = "Enter the port number your friend opened, like 25565.".into()
                }
                Err(e) => return self.join_error = e,
            }
        };
        if remotes.len() > peers::MAX_CHECKED {
            return self.join_error = format!("That's more than {} ports. Join them in smaller sets.", peers::MAX_CHECKED);
        }
        // A port this PC opens above: joining it here too (your own code, to
        // test it) would have TailCat's far end hand each connection straight
        // back to its near end, round and round.
        let hosted = |p: u16| self.hosts.iter().any(|h| h.proc.is_some() && h.ports.contains(&p));
        let mut maps = Vec::new();
        for (i, remote) in remotes.iter().enumerate() {
            let mut local = locals.get(i).copied().filter(|p| *p > 0).unwrap_or(*remote);
            if maps.iter().any(|(l, _)| *l == local) {
                return self.join_error = format!("Local port {local} is used twice. Give each one its own number.");
            }
            if !game_mode && hosted(local) {
                return self.join_error = format!("You opened port {local} above. Pick another local port for {remote}.");
            }
            // A game join picks another port itself (Minecraft finds it through
            // LAN anyway); a mount always does, as nobody types its address.
            if from == JoinFrom::Mount || (game_mode && (!port_free(local, lan) || hosted(local))) {
                local = match free_port() {
                    Ok(p) => p,
                    Err(e) => return self.join_error = e,
                };
            }
            if !port_free(local, lan) {
                return self.join_error =
                    format!("Port {local} is already in use on this PC. Pick another local port for {remote}.");
            }
            maps.push((local, *remote));
        }
        // TailCat always listens on 127.0.0.1 only. Shared on the network,
        // each picked port gets KittyConnect's own relay on every address,
        // passing to TailCat on a private port: TailCat's --bind would put
        // every port it listens on there, the beacon's too.
        let mut relays = Vec::new();
        let mut tc_maps = Vec::new();
        for (local, remote) in &maps {
            if !lan {
                tc_maps.push((*local, *remote));
                continue;
            }
            let near = match free_port() {
                Ok(p) => p,
                Err(e) => return self.join_error = e,
            };
            match loopback::LanRelay::start(*local, near) {
                Ok(r) => relays.push(r),
                Err(e) => return self.join_error = format!("Couldn't share port {local} on your network: {e}"),
            }
            tc_maps.push((near, *remote));
        }
        let mut args: Vec<String> = vec!["forward".into(), c.addr.clone()];
        args.extend(tc_maps.iter().map(|(l, r)| format!("{l}:{r}")));
        // A private local port to the host's beacon, for the 30-second check.
        let beacon = (!remotes.contains(&beacon::PORT)).then(free_port).and_then(Result::ok);
        if let Some(b) = beacon {
            args.push(format!("{b}:{}", beacon::PORT));
        }
        match tailcat::spawn(Role::PortJoin, &args, &self.proxy) {
            Ok(p) => {
                let key = self.key();
                let args = args.clone();
                let link = Link { next_ping: Some(Instant::now() + Duration::from_secs(4)), ..Link::default() };
                self.joins.insert(0, PortJoin {
                    key,
                    st: St::Starting,
                    error: String::new(),
                    proc: Some(p),
                    maps,
                    lan,
                    started: Instant::now(),
                    beacon,
                    told_unreachable: false,
                    ports_checked: false,
                    link,
                    args,
                    peer: peers::new_id(),
                    addr: c.addr,
                    host_name: None,
                    game,
                    worlds: Vec::new(),
                    lan_next: Instant::now() + Duration::from_secs(3),
                    lan_asking: false,
                    mount: (from == JoinFrom::Mount).then(MountJoin::default),
                    relays,
                    log: Log::default(),
                });
                self.set_status("Reaching your friend through TailCat…", "busy");
            }
            Err(e) => self.join_error = e,
        }
    }
}
