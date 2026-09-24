// lkl_proxy v8: std-only blocking proxy (no tokio/mio/epoll).
// 背景：v1-v4 已证明 tokio/mio 的 epoll 注册在 hijack 双 epoll 机制下 EBADF
// （注册落在未经 hook 建的 epfd 上），而 socket/bind/listen/accept/connect/
// read/write 全系可用（HAProxy 与全部探针为证）。故彻底弃用异步运行时，
// 只用宿主 C 库经 hijack 验证过的经典阻塞调用。
// v7 新增：LISTEN/BACKEND/MAXCONN/CONNECT_TIMEOUT_MS/RW_TIMEOUT_MS 走外部文件配置
// （KEY=VALUE，默认 /etc/lkl-haproxy/lkl-proxy.conf，可被环境变量覆盖）。
// v8 修复：两处 thread spawn 改 Builder+显式处理（失败拒绝本连接，不 panic）；
// BACKEND 只接受字面数字 IP，删 DNS fallback（fail-fast，与文档一致）。
use std::collections::HashMap;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_LISTEN: &str = "0.0.0.0:443";
const DEFAULT_BACKEND: &str = "10.0.0.1:443";
const DEFAULT_MAXCONN: usize = 20480;
// 与 haproxy.cfg 的 connect 5000 / client-server 50000ms 对齐。
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5000;
const DEFAULT_RW_TIMEOUT_MS: u64 = 50000;
// 外部配置文件默认路径（argv --config/-c > $LKL_PROXY_CONFIG > 此默认）。
const DEFAULT_CONFIG_PATH: &str = "/etc/lkl-haproxy/lkl-proxy.conf";

// 配置文件路径解析：argv 优先，其次环境变量，最后默认路径。
fn config_path() -> String {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "-c" || a == "--config" {
            if let Some(p) = args.next() {
                return p;
            }
        } else if let Some(p) = a.strip_prefix("--config=") {
            return p.to_string();
        }
    }
    if let Ok(p) = std::env::var("LKL_PROXY_CONFIG") {
        if !p.trim().is_empty() {
            return p;
        }
    }
    DEFAULT_CONFIG_PATH.to_string()
}

// 解析 KEY=VALUE 文件：空行/#;/;注释跳过，无 '=' 行告警忽略，值去首尾成对引号。
// 文件不存在视为“无文件配置”（兼容 v6 纯环境变量行为）；其他读取失败告警后回退。
fn load_kv_file(path: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return map,
        Err(e) => {
            eprintln!("lkl_proxy config {path} unreadable: {e}, fallback to env/defaults");
            return map;
        }
    };
    for (no, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some(eq) = line.find('=') else {
            eprintln!("lkl_proxy config {path}:{} ignored (no '='): {raw}", no + 1);
            continue;
        };
        let key = line[..eq].trim().to_string();
        let mut val = line[eq + 1..].trim().to_string();
        if val.len() >= 2
            && ((val.starts_with('"') && val.ends_with('"'))
                || (val.starts_with('\'') && val.ends_with('\'')))
        {
            val = val[1..val.len() - 1].to_string();
        }
        if key.is_empty() {
            eprintln!("lkl_proxy config {path}:{} ignored (empty key)", no + 1);
            continue;
        }
        map.insert(key, val);
    }
    map
}

// 取值优先级：环境变量（非空）> 配置文件 > 内置默认。返回 (值, 来源)。
fn pick(key: &str, file: &HashMap<String, String>, def: &str) -> (String, &'static str) {
    if let Ok(v) = std::env::var(key) {
        if !v.trim().is_empty() {
            return (v, "env");
        }
    }
    if let Some(v) = file.get(key) {
        if !v.trim().is_empty() {
            return (v.clone(), "file");
        }
    }
    (def.to_string(), "default")
}

fn parse_pos_u64(key: &str, s: &str) -> std::io::Result<u64> {
    match s.trim().parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => {
            eprintln!("lkl_proxy {key} invalid (must be positive integer): {s}");
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("bad {key}"),
            ))
        }
    }
}

fn main() -> std::io::Result<()> {
    // 配置文件必须在任何 socket 调用之前读完：hijack 只拦截 socket 系调用，
    // 普通文件 open/read 走宿主，此处读取不受劫持影响。
    let cfg_path = config_path();
    let file = load_kv_file(&cfg_path);

    let (listen, listen_src) = pick("LISTEN", &file, DEFAULT_LISTEN);
    let (backend, backend_src) = pick("BACKEND", &file, DEFAULT_BACKEND);
    let maxconn_def = DEFAULT_MAXCONN.to_string();
    let (maxconn_s, maxconn_src) = pick("MAXCONN", &file, &maxconn_def);
    let maxconn = parse_pos_u64("MAXCONN", &maxconn_s)? as usize;
    let conn_def = DEFAULT_CONNECT_TIMEOUT_MS.to_string();
    let (conn_s, conn_src) = pick("CONNECT_TIMEOUT_MS", &file, &conn_def);
    let connect_timeout = Duration::from_millis(parse_pos_u64("CONNECT_TIMEOUT_MS", &conn_s)?);
    let rw_def = DEFAULT_RW_TIMEOUT_MS.to_string();
    let (rw_s, rw_src) = pick("RW_TIMEOUT_MS", &file, &rw_def);
    let rw_timeout = Duration::from_millis(parse_pos_u64("RW_TIMEOUT_MS", &rw_s)?);

    // 先 parse 成数字地址，避免 ToSocketAddrs 走 DNS（hijack 下 DNS 不可用）。
    let listen_addr: std::net::SocketAddr = match listen.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("lkl_proxy LISTEN parse failed listen={listen}: {e}");
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, e));
        }
    };
    // BACKEND 只接受字面数字 IP：hijack 下 DNS 不可用，非法值直接 fail-fast。
    let backend_first: std::net::SocketAddr = match backend.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("lkl_proxy BACKEND parse failed backend={backend}: {e}");
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "bad BACKEND",
            ));
        }
    };

    let listener = TcpListener::bind(listen_addr).map_err(|e| {
        eprintln!("lkl_proxy BIND FAILED listen={listen}: {e:?}");
        e
    })?;
    eprintln!("lkl_proxy v8 std listen ok: {listen} backend={backend} maxconn={maxconn} cfg={cfg_path}[{listen_src},{backend_src},{maxconn_src},{conn_src},{rw_src}] connect_timeout_ms={} rw_timeout_ms={}", connect_timeout.as_millis(), rw_timeout.as_millis());

    let active = Arc::new(AtomicUsize::new(0));
    for inbound in listener.incoming() {
        let inbound = match inbound {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept failed: {e}");
                continue;
            }
        };
        if active.load(Ordering::SeqCst) >= maxconn {
            eprintln!("maxconn reached, reject");
            drop(inbound);
            continue;
        }
        active.fetch_add(1, Ordering::SeqCst);
        let active_sub = active.clone();
        // spawn 失败必须显式处理：顶层 spawn 失败默认 panic 会拖垮主进程。
        // Builder 失败时闭包连同 inbound 一起 drop，即拒绝本连接。
        if let Err(e) = std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || {
                let r = relay(inbound, backend_first, connect_timeout, rw_timeout);
                if let Err(e) = r {
                    eprintln!("relay failed: {e}");
                }
                active_sub.fetch_sub(1, Ordering::SeqCst);
            }) {
            eprintln!("thread spawn failed, rejecting connection: {e}");
            active.fetch_sub(1, Ordering::SeqCst);
        }
    }
    Ok(())
}

fn relay(
    client: TcpStream,
    backend: std::net::SocketAddr,
    connect_timeout: Duration,
    rw_timeout: Duration,
) -> std::io::Result<()> {
    let server = TcpStream::connect_timeout(&backend, connect_timeout).map_err(|e| {
        eprintln!("backend connect {backend} failed: {e}");
        e
    })?;
    // 超时是 socket 级选项，须在 clone 前设置，dup 出的 fd 共享同一 socket。
    client.set_read_timeout(Some(rw_timeout))?;
    client.set_write_timeout(Some(rw_timeout))?;
    server.set_read_timeout(Some(rw_timeout))?;
    server.set_write_timeout(Some(rw_timeout))?;

    let c_in = client.try_clone()?;
    let c_out = client;
    let s_in = server.try_clone()?;
    let s_out = server;
    let t = match std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut c_in = c_in;
            let mut s_out = s_out;
            let _ = std::io::copy(&mut c_in, &mut s_out);
            let _ = s_out.shutdown(Shutdown::Write);
        }) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("relay thread spawn failed: {e}");
            return Err(std::io::Error::new(
                std::io::ErrorKind::ResourceBusy,
                "relay thread spawn failed",
            ));
        }
    };
    let mut s_in = s_in;
    let mut c_out = c_out;
    let _ = std::io::copy(&mut s_in, &mut c_out);
    let _ = c_out.shutdown(Shutdown::Write);
    let _ = t.join();
    Ok(())
}
