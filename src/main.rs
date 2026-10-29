// netpeek - Kubernetes network diagnostics
// zero deps: stdlib only, shells out to iproute2 for interface detail

use std::env;
use std::fs;
use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
netpeek - Kubernetes network diagnostics

USAGE
  netpeek [flags] <command> [args]

COMMANDS
  info               netns snapshot: interfaces, routes, DNS config, listeners
  dns <name>         resolve a name via the system resolver, report addrs + rtt
  tcp <host:port>    TCP connect test, report latency or error class
  help               this text
  version            version string

FLAGS
  --json             machine-readable output
  --timeout <secs>   TCP connect timeout, default 5
  -v, --verbose      extra detail

EXIT CODES
  0 success   1 check failed   2 usage error
";

#[derive(Clone, Copy)]
struct Opts {
    json: bool,
    timeout: Duration,
    verbose: bool,
}

fn main() -> ExitCode {
    let argv: Vec<String> = env::args().skip(1).collect();
    let (opts, rest) = match parse_args(&argv) {
        Ok(v) => v,
        Err(m) => {
            eprintln!("netpeek: {m}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let cmd = match rest.first() {
        Some(c) => c.as_str(),
        None => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
    };
    match cmd {
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        "version" | "--version" => {
            println!("netpeek {VERSION}");
            ExitCode::SUCCESS
        }
        "info" => run_info(&opts),
        "dns" => match rest.get(1) {
            Some(n) => run_dns(&opts, n),
            None => {
                eprintln!("netpeek: dns needs a name, e.g. netpeek dns kubernetes.default.svc");
                ExitCode::from(2)
            }
        },
        "tcp" => match rest.get(1) {
            Some(t) => run_tcp(&opts, t),
            None => {
                eprintln!("netpeek: tcp needs <host:port>, e.g. netpeek tcp 10.96.0.1:443");
                ExitCode::from(2)
            }
        },
        other => {
            eprintln!("netpeek: unknown command '{other}'\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn parse_args(argv: &[String]) -> Result<(Opts, Vec<String>), String> {
    let mut o = Opts { json: false, timeout: Duration::from_secs(5), verbose: false };
    let mut rest = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--json" => o.json = true,
            "-v" | "--verbose" => o.verbose = true,
            "--timeout" => {
                i += 1;
                let v = argv.get(i).ok_or("--timeout needs seconds")?;
                let secs: u64 = v.parse().map_err(|_| format!("bad timeout '{v}'"))?;
                if secs == 0 {
                    return Err("timeout must be > 0".into());
                }
                o.timeout = Duration::from_secs(secs);
            }
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    Ok((o, rest))
}

// ---------- info ----------

struct DnsCfg {
    nameservers: Vec<String>,
    search: Vec<String>,
    ndots: String,
}

fn parse_resolv(txt: &str) -> DnsCfg {
    let mut c = DnsCfg { nameservers: vec![], search: vec![], ndots: "1".into() };
    for line in txt.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let mut it = line.split_whitespace();
        match it.next() {
            Some("nameserver") => {
                if let Some(v) = it.next() {
                    c.nameservers.push(v.to_string());
                }
            }
            Some("search") | Some("domain") => {
                for v in it {
                    c.search.push(v.to_string());
                }
            }
            Some("options") => {
                for v in it {
                    if let Some(n) = v.strip_prefix("ndots:") {
                        c.ndots = n.to_string();
                    }
                }
            }
            _ => {}
        }
    }
    c
}

struct Iface {
    name: String,
    mtu: String,
    state: String,
    addrs: Vec<String>,
}

fn ip_cmd(args: &[&str]) -> Option<String> {
    let out = Command::new("ip").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn read_trim(p: &str) -> String {
    fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn iface_line(line: &str) -> Option<(String, Option<String>)> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    if toks.len() < 2 {
        return None;
    }
    let name = toks[1].trim_end_matches(':').to_string();
    let addr = toks
        .iter()
        .position(|t| *t == "inet" || *t == "inet6")
        .and_then(|p| toks.get(p + 1))
        .map(|s| s.to_string());
    Some((name, addr))
}

fn ifaces() -> Option<Vec<Iface>> {
    let out = ip_cmd(&["-o", "addr", "show"])?;
    let mut list: Vec<Iface> = Vec::new();
    for line in out.lines() {
        let (name, addr) = match iface_line(line) {
            Some(v) => v,
            None => continue,
        };
        match list.iter_mut().find(|i| i.name == name) {
            Some(e) => {
                if let Some(a) = addr {
                    e.addrs.push(a);
                }
            }
            None => {
                let sys = format!("/sys/class/net/{name}");
                list.push(Iface {
                    addrs: addr.into_iter().collect(),
                    mtu: read_trim(&format!("{sys}/mtu")),
                    state: read_trim(&format!("{sys}/operstate")),
                    name,
                });
            }
        }
    }
    Some(list)
}

fn routes() -> Option<Vec<String>> {
    let out = ip_cmd(&["-o", "route", "show"])?;
    Some(out.lines().map(|l| l.trim().to_string()).collect())
}

fn hex_ipv4(h: &str) -> String {
    match u32::from_str_radix(h, 16) {
        Ok(v) => {
            let b = v.to_le_bytes();
            format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
        }
        Err(_) => h.to_string(),
    }
}

fn listen_line(line: &str, v6: bool) -> Option<String> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 4 || f[3] != "0A" {
        return None;
    }
    let mut la = f[1].split(':');
    let ip = la.next().unwrap_or("");
    let port = la.next().and_then(|p| u16::from_str_radix(p, 16).ok())?;
    let host = if v6 { "::".to_string() } else { hex_ipv4(ip) };
    Some(format!("tcp  {host}:{port}"))
}

fn listeners() -> Vec<String> {
    let mut v = Vec::new();
    for (path, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let txt = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for line in txt.lines().skip(1) {
            if let Some(s) = listen_line(line, v6) {
                v.push(s);
            }
        }
    }
    v
}

fn run_info(o: &Opts) -> ExitCode {
    let hostname = read_trim("/proc/sys/kernel/hostname");
    let kernel = read_trim("/proc/sys/kernel/osrelease");
    let dns = parse_resolv(&fs::read_to_string("/etc/resolv.conf").unwrap_or_default());
    let ifs = ifaces();
    let rts = routes().unwrap_or_default();
    let lis = listeners();

    if o.json {
        let if_json: Vec<String> = ifs
            .as_ref()
            .map(|l| {
                l.iter()
                    .map(|i| {
                        format!(
                            "{{\"name\":{},\"mtu\":{},\"state\":{},\"addrs\":{}}}",
                            jstr(&i.name),
                            jstr(&i.mtu),
                            jstr(&i.state),
                            jarr(&i.addrs)
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        println!(
            "{{\"command\":\"info\",\"hostname\":{},\"kernel\":{},\"interfaces\":[{}],\"routes\":{},\"dns\":{{\"nameservers\":{},\"search\":{},\"ndots\":{}}},\"listeners\":{}}}",
            jstr(&hostname),
            jstr(&kernel),
            if_json.join(","),
            jarr(&rts),
            jarr(&dns.nameservers),
            jarr(&dns.search),
            jstr(&dns.ndots),
            jarr(&lis)
        );
        return ExitCode::SUCCESS;
    }

    println!("netpeek info");
    println!("  hostname    {hostname}");
    println!("  kernel      {kernel}");

    println!("\ninterfaces");
    match &ifs {
        None => println!("  iproute2 not found in PATH (install iproute2)"),
        Some(l) if l.is_empty() => println!("  none"),
        Some(l) => {
            for i in l {
                println!(
                    "  {:<8} mtu {:<6} {:<8} {}",
                    i.name,
                    i.mtu,
                    i.state,
                    i.addrs.join(", ")
                );
            }
        }
    }

    println!("\nroutes");
    if rts.is_empty() {
        println!("  none");
    } else {
        for r in &rts {
            println!("  {r}");
        }
    }

    println!("\ndns");
    println!("  nameservers {}", if dns.nameservers.is_empty() { "-".to_string() } else { dns.nameservers.join(" ") });
    println!("  search      {}", if dns.search.is_empty() { "-".to_string() } else { dns.search.join(" ") });
    println!("  ndots       {}", dns.ndots);

    println!("\nlisteners");
    if lis.is_empty() {
        println!("  none");
    } else {
        for l in &lis {
            println!("  {l}");
        }
    }
    ExitCode::SUCCESS
}

// ---------- dns ----------

fn run_dns(o: &Opts, name: &str) -> ExitCode {
    let dns = parse_resolv(&fs::read_to_string("/etc/resolv.conf").unwrap_or_default());
    let t0 = Instant::now();
    let res = (name, 0u16).to_socket_addrs();
    let rtt = t0.elapsed();

    match res {
        Ok(iter) => {
            let mut addrs: Vec<String> = iter.map(|a| a.ip().to_string()).collect();
            addrs.sort();
            addrs.dedup();
            if o.json {
                println!(
                    "{{\"command\":\"dns\",\"name\":{},\"ok\":true,\"addresses\":{},\"rtt_ms\":{:.3},\"nameservers\":{},\"search\":{}}}",
                    jstr(name),
                    jarr(&addrs),
                    rtt.as_secs_f64() * 1000.0,
                    jarr(&dns.nameservers),
                    jarr(&dns.search)
                );
            } else {
                println!("netpeek dns {name}");
                println!("  status      ok");
                println!("  addresses   {}", addrs.join(", "));
                println!("  rtt         {:.1} ms", rtt.as_secs_f64() * 1000.0);
                println!("  nameservers {}", if dns.nameservers.is_empty() { "-".to_string() } else { dns.nameservers.join(" ") });
                println!("  search      {}", if dns.search.is_empty() { "-".to_string() } else { dns.search.join(" ") });
                println!("  ndots       {}", dns.ndots);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if o.json {
                println!(
                    "{{\"command\":\"dns\",\"name\":{},\"ok\":false,\"error\":{},\"rtt_ms\":{:.3},\"nameservers\":{}}}",
                    jstr(name),
                    jstr(&e.to_string()),
                    rtt.as_secs_f64() * 1000.0,
                    jarr(&dns.nameservers)
                );
            } else {
                println!("netpeek dns {name}");
                println!("  status      FAILED");
                println!("  error       {e}");
                println!("  rtt         {:.1} ms", rtt.as_secs_f64() * 1000.0);
                println!("  nameservers {}", if dns.nameservers.is_empty() { "-".to_string() } else { dns.nameservers.join(" ") });
            }
            ExitCode::from(1)
        }
    }
}

// ---------- tcp ----------

fn split_target(t: &str) -> Option<(String, u16)> {
    let (h, p) = t.rsplit_once(':')?;
    let port: u16 = p.parse().ok()?;
    let host = h.trim_start_matches('[').trim_end_matches(']').to_string();
    if host.is_empty() || port == 0 {
        return None;
    }
    Some((host, port))
}

fn err_class(e: &io::Error) -> &'static str {
    match e.kind() {
        io::ErrorKind::ConnectionRefused => "refused",
        io::ErrorKind::TimedOut => "timeout",
        io::ErrorKind::PermissionDenied => "permission-denied",
        io::ErrorKind::AddrNotAvailable => "no-local-addr",
        _ => match e.raw_os_error() {
            Some(101) => "network-unreachable",
            Some(113) => "host-unreachable",
            Some(111) => "refused",
            Some(110) => "timeout",
            _ => "error",
        },
    }
}

fn run_tcp(o: &Opts, target: &str) -> ExitCode {
    let (host, port) = match split_target(target) {
        Some(v) => v,
        None => {
            eprintln!("netpeek: bad target '{target}', want <host:port>");
            return ExitCode::from(2);
        }
    };

    let addrs: Vec<SocketAddr> = match (host.as_str(), port).to_socket_addrs() {
        Ok(it) => it.collect(),
        Err(e) => {
            if o.json {
                println!(
                    "{{\"command\":\"tcp\",\"target\":{},\"ok\":false,\"stage\":\"resolve\",\"error\":{}}}",
                    jstr(target),
                    jstr(&e.to_string())
                );
            } else {
                println!("netpeek tcp {target}");
                println!("  status      FAILED (resolve)");
                println!("  error       {e}");
            }
            return ExitCode::from(1);
        }
    };

    let mut results: Vec<(SocketAddr, bool, u128, String)> = Vec::new();
    let mut ok_any = false;
    for a in &addrs {
        let t0 = Instant::now();
        match TcpStream::connect_timeout(a, o.timeout) {
            Ok(_) => {
                ok_any = true;
                results.push((*a, true, t0.elapsed().as_millis(), String::new()));
            }
            Err(e) => results.push((*a, false, t0.elapsed().as_millis(), err_class(&e).to_string())),
        }
    }

    if o.json {
        let rows: Vec<String> = results
            .iter()
            .map(|(a, ok, ms, cls)| {
                format!(
                    "{{\"addr\":{},\"ok\":{},\"latency_ms\":{},\"error\":{}}}",
                    jstr(&a.to_string()),
                    ok,
                    ms,
                    if cls.is_empty() { "null".to_string() } else { jstr(cls) }
                )
            })
            .collect();
        println!(
            "{{\"command\":\"tcp\",\"target\":{},\"ok\":{},\"attempts\":[{}]}}",
            jstr(target),
            ok_any,
            rows.join(",")
        );
    } else {
        println!("netpeek tcp {target}");
        println!("  status      {}", if ok_any { "ok" } else { "FAILED" });
        for (a, ok, ms, cls) in &results {
            if *ok {
                println!("  {a}  connected in {ms} ms");
            } else {
                println!("  {a}  failed ({cls}) after {ms} ms");
            }
        }
    }

    if ok_any {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

// ---------- json ----------

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

fn jstr(s: &str) -> String {
    format!("\"{}\"", esc(s))
}

fn jarr(v: &[String]) -> String {
    let items: Vec<String> = v.iter().map(|s| jstr(s)).collect();
    format!("[{}]", items.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escapes() {
        assert_eq!(esc("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(esc("\u{1}"), "\\u0001");
        assert_eq!(jarr(&["x".into(), "y".into()]), "[\"x\",\"y\"]");
    }

    #[test]
    fn resolv_parsed() {
        let c = parse_resolv(
            "# comment\nnameserver 10.96.0.10\nnameserver 1.1.1.1\nsearch default.svc.cluster.local svc.cluster.local\noptions ndots:5 timeout:2\n",
        );
        assert_eq!(c.nameservers, vec!["10.96.0.10", "1.1.1.1"]);
        assert_eq!(c.search, vec!["default.svc.cluster.local", "svc.cluster.local"]);
        assert_eq!(c.ndots, "5");
    }

    #[test]
    fn target_split() {
        assert_eq!(split_target("10.96.0.1:443"), Some(("10.96.0.1".into(), 443)));
        assert_eq!(split_target("[::1]:8080"), Some(("::1".into(), 8080)));
        assert_eq!(split_target("host"), None);
        assert_eq!(split_target("host:0"), None);
        assert_eq!(split_target(":80"), None);
    }

    #[test]
    fn hex_addr_decodes() {
        assert_eq!(hex_ipv4("0100007F"), "127.0.0.1");
        assert_eq!(hex_ipv4("00000000"), "0.0.0.0");
        assert_eq!(hex_ipv4("zz"), "zz");
    }

    #[test]
    fn refused_maps() {
        let e = io::Error::from_raw_os_error(111);
        assert_eq!(err_class(&e), "refused");
        let e = io::Error::from_raw_os_error(101);
        assert_eq!(err_class(&e), "network-unreachable");
    }

    #[test]
    fn iface_lines_parsed() {
        let lo = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever";
        assert_eq!(iface_line(lo), Some(("lo".into(), Some("127.0.0.1/8".into()))));
        let eth = "2: eth0    inet 172.17.0.3/16 brd 172.17.255.255 scope global eth0\\       valid_lft forever preferred_lft forever";
        assert_eq!(iface_line(eth), Some(("eth0".into(), Some("172.17.0.3/16".into()))));
        let v6 = "1: lo    inet6 ::1/128 scope host proto kernel_lo \\       valid_lft forever preferred_lft forever";
        assert_eq!(iface_line(v6), Some(("lo".into(), Some("::1/128".into()))));
        assert_eq!(iface_line(""), None);
    }

    #[test]
    fn listen_lines_parsed() {
        let hdr = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode";
        assert_eq!(listen_line(hdr, false), None);
        let listen = "   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1";
        assert_eq!(listen_line(listen, false), Some("tcp  0.0.0.0:8080".to_string()));
        let established = "   1: 0100007F:0016 0100007F:D431 01 00000000:00000000 00:00000000 00000000     0        0 12346 1";
        assert_eq!(listen_line(established, false), None);
    }
}
