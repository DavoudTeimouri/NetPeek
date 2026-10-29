// netpeek - Kubernetes network diagnostics
// zero deps: stdlib only, shells out to iproute2 for interface detail

use std::env;
use std::fs;
use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::{Command, ExitCode, Stdio};
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
  http <url>         HTTP(S) request: status, timings, TLS chain and cert expiry
  svc <name> [-n ns] Service -> ClusterIP -> endpoint slices -> per-backend probe
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
        "http" => match rest.get(1) {
            Some(u) => run_http(&opts, u),
            None => {
                eprintln!("netpeek: http needs a url, e.g. netpeek http https://kubernetes.default.svc/healthz");
                ExitCode::from(2)
            }
        },
        "svc" => {
            if rest.len() < 2 {
                eprintln!("netpeek: svc needs a service name, e.g. netpeek svc kubernetes -n default");
                ExitCode::from(2)
            } else {
                run_svc(&opts, &rest[1..])
            }
        }
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

// ---------- http ----------

struct Url {
    scheme: String,
    host: String,
    port: u16,
}

fn parse_url(u: &str) -> Option<Url> {
    let (scheme, rest) = u.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let hostport = rest.split(|c: char| c == '/' || c == '?' || c == '#').next()?;
    if hostport.is_empty() {
        return None;
    }
    let unbracket = |s: &str| s.trim_start_matches('[').trim_end_matches(']').to_string();
    let default_port = if scheme == "https" { 443 } else { 80 };
    // IPv6 literals must be bracketed: http://[::1]:8080/
    match hostport.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => {
            Some(Url { scheme, host: unbracket(h), port: p.parse().ok()? })
        }
        _ => Some(Url { scheme, host: unbracket(hostport), port: default_port }),
    }
}

struct CurlRes {
    code: String,
    remote: String,
    dns_ms: f64,
    connect_ms: f64,
    tls_ms: f64,
    ttfb_ms: f64,
    total_ms: f64,
}

fn s2ms(s: &str) -> f64 {
    s.parse::<f64>().unwrap_or(0.0) * 1000.0
}

fn curl_timings(o: &Opts, url: &str) -> Result<CurlRes, String> {
    let fmt = "%{http_code}|%{remote_ip}|%{time_namelookup}|%{time_connect}|%{time_appconnect}|%{time_starttransfer}|%{time_total}";
    let args: Vec<String> = vec![
        "-sS".into(),
        "-o".into(),
        "/dev/null".into(),
        "--max-time".into(),
        o.timeout.as_secs().to_string(),
        "-w".into(),
        fmt.into(),
        url.into(),
    ];
    let out = Command::new("curl").args(&args).output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("curl exit {:?}", out.status.code())
        } else {
            err
        });
    }
    let body = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let f: Vec<&str> = body.split('|').collect();
    if f.len() < 7 {
        return Err(format!("unexpected curl output: {body}"));
    }
    Ok(CurlRes {
        code: f[0].to_string(),
        remote: f[1].to_string(),
        dns_ms: s2ms(f[2]),
        connect_ms: s2ms(f[3]),
        tls_ms: s2ms(f[4]),
        ttfb_ms: s2ms(f[5]),
        total_ms: s2ms(f[6]),
    })
}

fn openssl_x509(host: &str, port: u16, args: &[&str]) -> Option<(bool, String)> {
    let sargs: Vec<String> = vec![
        "s_client".into(),
        "-servername".into(),
        host.into(),
        "-connect".into(),
        format!("{host}:{port}"),
    ];
    let mut sc = Command::new("openssl")
        .args(&sargs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let out = Command::new("openssl")
        .arg("x509")
        .arg("-noout")
        .args(args)
        .stdin(Stdio::from(sc.stdout.take()?))
        .output()
        .ok()?;
    let _ = sc.wait();
    Some((out.status.success(), String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

fn cert_field(txt: &str, key: &str) -> Option<String> {
    txt.lines().find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_string()))
}

fn cert_san(txt: &str) -> Option<String> {
    let mut it = txt.lines();
    while let Some(l) = it.next() {
        if l.contains("Subject Alternative Name") {
            return it.next().map(|s| s.trim().to_string());
        }
    }
    None
}

fn run_http(o: &Opts, url: &str) -> ExitCode {
    let u = match parse_url(url) {
        Some(v) => v,
        None => {
            eprintln!("netpeek: bad url '{url}', want http(s)://host[:port][/path]");
            return ExitCode::from(2);
        }
    };

    let r = match curl_timings(o, url) {
        Ok(v) => v,
        Err(e) => {
            if o.json {
                println!(
                    "{{\"command\":\"http\",\"url\":{},\"ok\":false,\"error\":{}}}",
                    jstr(url),
                    jstr(&e)
                );
            } else {
                println!("netpeek http {url}");
                println!("  status      FAILED");
                println!("  error       {e}");
            }
            return ExitCode::from(1);
        }
    };

    let mut not_after = None;
    let mut san = None;
    let mut expired = None;
    let mut issuer = None;
    if u.scheme == "https" {
        if let Some((true, txt)) =
            openssl_x509(&u.host, u.port, &["-subject", "-issuer", "-dates", "-ext", "subjectAltName"])
        {
            not_after = cert_field(&txt, "notAfter=");
            issuer = cert_field(&txt, "issuer=");
            san = cert_san(&txt);
        }
        expired = openssl_x509(&u.host, u.port, &["-checkend", "0"]).and_then(|(_, t)| {
            if t.contains("will not expire") {
                Some(false)
            } else if t.contains("will expire") {
                Some(true)
            } else {
                None
            }
        });
    }

    if o.json {
        println!(
            "{{\"command\":\"http\",\"url\":{},\"ok\":true,\"status\":{},\"remote\":{},\"timings_ms\":{{\"dns\":{:.3},\"connect\":{:.3},\"tls\":{:.3},\"ttfb\":{:.3},\"total\":{:.3}}},\"tls\":{{\"not_after\":{},\"expired\":{},\"issuer\":{},\"san\":{}}}}}",
            jstr(url),
            jstr(&r.code),
            jstr(&r.remote),
            r.dns_ms,
            r.connect_ms,
            r.tls_ms,
            r.ttfb_ms,
            r.total_ms,
            opt_json(&not_after),
            expired.map(|b| b.to_string()).unwrap_or_else(|| "null".into()),
            opt_json(&issuer),
            opt_json(&san)
        );
    } else {
        println!("netpeek http {url}");
        println!("  status      {} ({})", r.code, if r.code.starts_with('2') { "ok" } else { "not 2xx" });
        println!("  remote      {}", r.remote);
        println!(
            "  timings     dns {:.1}ms  connect {:.1}ms  tls {:.1}ms  ttfb {:.1}ms  total {:.1}ms",
            r.dns_ms, r.connect_ms, r.tls_ms, r.ttfb_ms, r.total_ms
        );
        if u.scheme == "https" {
            if let Some(na) = &not_after {
                println!(
                    "  cert        expires {na}{}",
                    match expired {
                        Some(true) => "  EXPIRED",
                        Some(false) => "",
                        None => "",
                    }
                );
            }
            if let Some(i) = &issuer {
                println!("  issuer      {i}");
            }
            if let Some(s) = &san {
                println!("  san         {s}");
            }
        }
    }

    if r.code.starts_with('2') || r.code.starts_with('3') {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

// ---------- svc ----------

const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
const API_HOST: &str = "https://kubernetes.default.svc";

fn api_get(path: &str) -> Result<String, String> {
    let token = fs::read_to_string(format!("{SA_DIR}/token"))
        .map_err(|_| format!("no service account token at {SA_DIR}/token - not running inside a pod?"))?
        .trim()
        .to_string();
    let args: Vec<String> = vec![
        "-sS".into(),
        "--fail-with-body".into(),
        "--max-time".into(),
        "10".into(),
        "--cacert".into(),
        format!("{SA_DIR}/ca.crt"),
        "-H".into(),
        format!("Authorization: Bearer {token}"),
        "-H".into(),
        "Accept: application/json".into(),
        format!("{API_HOST}{path}"),
    ];
    let out = Command::new("curl").args(&args).output().map_err(|e| format!("curl: {e}"))?;
    let body = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["message"].as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| body.trim().chars().take(200).collect());
        return Err(format!("GET {path}: {msg}"));
    }
    Ok(body)
}

struct Backend {
    ip: String,
    port: u16,
    ready: bool,
}

fn svc_summary(json: &str) -> Result<(String, Vec<(String, u16, String)>), String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let ip = v["spec"]["clusterIP"].as_str().unwrap_or("-").to_string();
    let mut ports = Vec::new();
    if let Some(arr) = v["spec"]["ports"].as_array() {
        for p in arr {
            ports.push((
                p["name"].as_str().unwrap_or("").to_string(),
                p["port"].as_u64().unwrap_or(0) as u16,
                p["protocol"].as_str().unwrap_or("TCP").to_string(),
            ));
        }
    }
    Ok((ip, ports))
}

fn slice_backends(json: &str) -> Result<Vec<Backend>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(items) = v["items"].as_array() {
        for it in items {
            let mut ports: Vec<u16> = Vec::new();
            if let Some(ps) = it["ports"].as_array() {
                for p in ps {
                    if let Some(n) = p["port"].as_u64() {
                        ports.push(n as u16);
                    }
                }
            }
            if let Some(eps) = it["endpoints"].as_array() {
                for ep in eps {
                    let ready = ep["conditions"]["ready"].as_bool().unwrap_or(false);
                    if let Some(addrs) = ep["addresses"].as_array() {
                        for a in addrs {
                            if let Some(ip) = a.as_str() {
                                for p in &ports {
                                    out.push(Backend { ip: ip.to_string(), port: *p, ready });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| (a.ip.as_str(), a.port).cmp(&(b.ip.as_str(), b.port)));
    out.dedup_by(|a, b| a.ip == b.ip && a.port == b.port);
    Ok(out)
}

fn probe(ip: &str, port: u16, timeout: Duration) -> (bool, u128, String) {
    let addr = match format!("{ip}:{port}").parse::<SocketAddr>() {
        Ok(a) => a,
        Err(_) => return (false, 0, "bad-addr".to_string()),
    };
    let t0 = Instant::now();
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => (true, t0.elapsed().as_millis(), String::new()),
        Err(e) => (false, t0.elapsed().as_millis(), err_class(&e).to_string()),
    }
}

fn run_svc(o: &Opts, args: &[String]) -> ExitCode {
    let mut name: Option<String> = None;
    let mut ns: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" | "--namespace" => {
                i += 1;
                match args.get(i) {
                    Some(v) => ns = Some(v.clone()),
                    None => {
                        eprintln!("netpeek: -n needs a namespace");
                        return ExitCode::from(2);
                    }
                }
            }
            a if a.starts_with('-') => {
                eprintln!("netpeek: unknown flag {a}");
                return ExitCode::from(2);
            }
            a => {
                if name.is_none() {
                    name = Some(a.to_string());
                }
            }
        }
        i += 1;
    }
    let name = match name {
        Some(v) => v,
        None => {
            eprintln!("netpeek: svc needs a name");
            return ExitCode::from(2);
        }
    };
    let ns = match ns {
        Some(v) => v,
        None => {
            let d = read_trim(&format!("{SA_DIR}/namespace"));
            if d.is_empty() { "default".to_string() } else { d }
        }
    };

    let svc_json = match api_get(&format!("/api/v1/namespaces/{ns}/services/{name}")) {
        Ok(v) => v,
        Err(e) => return svc_fail(o, &name, &ns, &e),
    };
    let (cluster_ip, ports) = match svc_summary(&svc_json) {
        Ok(v) => v,
        Err(e) => return svc_fail(o, &name, &ns, &format!("bad service JSON: {e}")),
    };

    let slice_path = format!(
        "/apis/discovery.k8s.io/v1/namespaces/{ns}/endpointslices?labelSelector=kubernetes.io/service-name%3D{name}"
    );
    let backends = match api_get(&slice_path).and_then(|j| slice_backends(&j)) {
        Ok(v) => v,
        Err(e) => return svc_fail(o, &name, &ns, &e),
    };

    let mut rows: Vec<(String, u16, bool, bool, u128, String)> = Vec::new();
    for b in &backends {
        let (ok, ms, cls) = probe(&b.ip, b.port, o.timeout);
        rows.push((b.ip.clone(), b.port, b.ready, ok, ms, cls));
    }
    let ready: Vec<&(String, u16, bool, bool, u128, String)> = rows.iter().filter(|r| r.2).collect();
    let all_ok = !ready.is_empty() && ready.iter().all(|r| r.3);

    if o.json {
        let jrows: Vec<String> = rows
            .iter()
            .map(|(ip, port, rdy, ok, ms, cls)| {
                format!(
                    "{{\"ip\":{},\"port\":{},\"ready\":{},\"connected\":{},\"latency_ms\":{},\"error\":{}}}",
                    jstr(ip),
                    port,
                    rdy,
                    ok,
                    ms,
                    if cls.is_empty() { "null".to_string() } else { jstr(cls) }
                )
            })
            .collect();
        let jports: Vec<String> = ports
            .iter()
            .map(|(n, p, pr)| {
                format!("{{\"name\":{},\"port\":{},\"protocol\":{}}}", jstr(n), p, jstr(pr))
            })
            .collect();
        println!(
            "{{\"command\":\"svc\",\"name\":{},\"namespace\":{},\"cluster_ip\":{},\"ports\":[{}],\"endpoints\":[{}],\"ok\":{}}}",
            jstr(&name),
            jstr(&ns),
            jstr(&cluster_ip),
            jports.join(","),
            jrows.join(","),
            all_ok
        );
    } else {
        println!("netpeek svc {name} -n {ns}");
        println!("  clusterIP   {cluster_ip}");
        let pdesc: Vec<String> = ports.iter().map(|(n, p, pr)| {
            if n.is_empty() { format!("{p}/{pr}") } else { format!("{n}:{p}/{pr}") }
        }).collect();
        println!("  ports       {}", if pdesc.is_empty() { "-".to_string() } else { pdesc.join(", ") });
        println!(
            "  endpoints   {} ready, {} not-ready",
            rows.iter().filter(|r| r.2).count(),
            rows.iter().filter(|r| !r.2).count()
        );
        if rows.is_empty() {
            println!("  no endpoints - selector matches nothing, or the slice RBAC is missing");
        }
        for (ip, port, rdy, ok, ms, cls) in &rows {
            let state = if *rdy { "ready  " } else { "not-rdy" };
            if *ok {
                println!("  {ip}:{port}  {state}  connected in {ms} ms");
            } else {
                println!("  {ip}:{port}  {state}  FAILED ({cls}) after {ms} ms");
            }
        }
    }

    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn svc_fail(o: &Opts, name: &str, ns: &str, err: &str) -> ExitCode {
    if o.json {
        println!(
            "{{\"command\":\"svc\",\"name\":{},\"namespace\":{},\"ok\":false,\"error\":{}}}",
            jstr(name),
            jstr(ns),
            jstr(err)
        );
    } else {
        println!("netpeek svc {name} -n {ns}");
        println!("  status      FAILED");
        println!("  error       {err}");
    }
    ExitCode::from(1)
}

fn opt_json(v: &Option<String>) -> String {
    match v {
        Some(s) => jstr(s),
        None => "null".to_string(),
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

    #[test]
    fn url_parsed() {
        let u = parse_url("https://kubernetes.default.svc/healthz").unwrap();
        assert_eq!((u.scheme.as_str(), u.host.as_str(), u.port), ("https", "kubernetes.default.svc", 443));
        let u = parse_url("http://10.96.0.1:8080/x?y=1").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("10.96.0.1", 8080));
        let u = parse_url("https://[::1]:8443/").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 8443));
        assert!(parse_url("ftp://x/").is_none());
        assert!(parse_url("no-scheme").is_none());
    }

    #[test]
    fn svc_json_parsed() {
        let j = r#"{"spec":{"clusterIP":"10.96.14.7","ports":[{"name":"http","port":80,"protocol":"TCP"},{"port":9090,"protocol":"TCP"}]}}"#;
        let (ip, ports) = svc_summary(j).unwrap();
        assert_eq!(ip, "10.96.14.7");
        assert_eq!(
            ports,
            vec![
                ("http".to_string(), 80u16, "TCP".to_string()),
                (String::new(), 9090u16, "TCP".to_string())
            ]
        );
    }

    #[test]
    fn slices_parsed_and_deduped() {
        let j = r#"{"items":[
            {"ports":[{"port":8080}],"endpoints":[
                {"addresses":["10.244.1.5"],"conditions":{"ready":true}},
                {"addresses":["10.244.2.7"],"conditions":{"ready":false}}]},
            {"ports":[{"port":8080}],"endpoints":[{"addresses":["10.244.1.5"],"conditions":{"ready":true}}]}]}"#;
        let b = slice_backends(j).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].ip.as_str(), b[0].port, b[0].ready), ("10.244.1.5", 8080, true));
        assert_eq!((b[1].ip.as_str(), b[1].port, b[1].ready), ("10.244.2.7", 8080, false));
    }

    #[test]
    fn cert_fields_parsed() {
        let t = "subject=CN = kubernetes\nissuer=CN = kubernetes\nnotBefore=Oct 28 06:00:00 2026 GMT\nnotAfter=Oct 28 06:00:00 2027 GMT\nX509v3 Subject Alternative Name: \n    DNS:kubernetes, DNS:kubernetes.default\n";
        assert_eq!(cert_field(t, "notAfter=").as_deref(), Some("Oct 28 06:00:00 2027 GMT"));
        assert_eq!(cert_san(t).as_deref(), Some("DNS:kubernetes, DNS:kubernetes.default"));
        assert_eq!(cert_field(t, "notThere="), None);
    }
}
