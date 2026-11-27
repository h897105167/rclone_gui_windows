//! 假的 K-Vault 站点：用户真实站点会应答的那个 HTTP 面。
//!
//! 跑在 `127.0.0.1` 的临时端口上，用一个普通 OS 线程接连接——测试本身是异步的，
//! 但这里不需要运行时，写一个阻塞的 accept 循环比在测试里塞个 HTTP 服务器好懂。
//!
//! 它只回答插件解析器认的那几种形状（见插件仓库的 `api.rs`），并且把收到的上传
//! 整份记下来：宿主发出去的 multipart 长什么样、字段是不是排在文件前面、
//! `Idempotency-Key` 前缀对不对，这几件事只有在这里看得见。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde_json::{json, Value};

/// 假站点声称自己存着的文件。分两页给，第二页要跟着游标才会走到。
fn catalog() -> Vec<Value> {
    vec![
        json!({
            "id": "f1",
            "name": "photo.png",
            "folderPath": "album",
            "size": 10,
            "uploadedAt": "2026-01-02T03:04:05Z",
        }),
        json!({
            "id": "f2",
            "name": "big.bin",
            "folderPath": "album/2026",
            "size": 2048,
            "uploadedAt": "2026-02-03T04:05:06Z",
        }),
        json!({
            "id": "f3",
            "name": "notes.txt",
            "folderPath": "album",
            "size": 5,
            "uploadedAt": "2026-03-04T05:06:07Z",
        }),
    ]
}

/// 一次上传的收件记录：测试要核字节、核字段顺序、核幂等键。
#[derive(Clone, Debug, Default)]
pub struct Received {
    pub path: String,
    pub idempotency_key: String,
    pub content_type: String,
    /// 表单字段，按到达顺序；文件段不算在内。
    pub fields: Vec<(String, String)>,
    pub file_field: String,
    pub file_name: String,
    pub file_bytes: Vec<u8>,
    /// 第一个表单字段在 multipart 正文里的偏移，以及文件段的偏移。
    /// 前者更小，就说明字段排在了文件前面。
    pub field_offset: usize,
    pub file_offset: usize,
}

impl Received {
    /// 所有文本字段都排在文件之前。宿主答应这么做，是为了让服务端在收字节
    /// 之前就知道该往哪儿存。
    pub fn fields_before_file(&self) -> bool {
        self.field_offset < self.file_offset
    }
}

/// 站点被要求删除的东西。
#[derive(Clone, Debug)]
struct Delete {
    id: String,
}

pub struct MockSite {
    addr: SocketAddr,
    received: Arc<Mutex<Vec<Received>>>,
    deleted: Arc<Mutex<Vec<Delete>>>,
    hits: Arc<Mutex<Vec<String>>>,
    _thread: JoinHandle<()>,
}

impl MockSite {
    pub fn start() -> MockSite {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本机端口失败");
        let addr = listener.local_addr().expect("取端口失败");
        let received: Arc<Mutex<Vec<Received>>> = Arc::new(Mutex::new(Vec::new()));
        let deleted: Arc<Mutex<Vec<Delete>>> = Arc::new(Mutex::new(Vec::new()));
        let hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (r, d, h) = (received.clone(), deleted.clone(), hits.clone());
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (r, d, h) = (r.clone(), d.clone(), h.clone());
                std::thread::spawn(move || {
                    let _ = serve(stream, &r, &d, &h);
                });
            }
        });
        MockSite {
            addr,
            received,
            deleted,
            hits,
            _thread: thread,
        }
    }

    /// K-Vault 远程指向的那个只读 DAV 地址。
    pub fn dav_url(&self) -> String {
        format!("http://{}/dav", self.addr)
    }

    /// 站点根，插件就是从 DAV 地址上退到这一层的。
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn uploads(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    pub fn deleted_ids(&self) -> Vec<String> {
        self.deleted
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.id.clone())
            .collect()
    }

    /// 收到的每一条请求行，形如 `METHOD path`。
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
}

fn serve(
    mut stream: TcpStream,
    received: &Mutex<Vec<Received>>,
    deleted: &Mutex<Vec<Delete>>,
    hits: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body)?;
    }

    hits.lock().unwrap().push(format!("{method} {path}"));

    let (status, payload) = route(&method, &path, &headers, &body, received, deleted);
    let text = payload.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        text.len(),
        text
    )?;
    stream.flush()
}

fn route(
    method: &str,
    path: &str,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    received: &Mutex<Vec<Received>>,
    deleted: &Mutex<Vec<Delete>>,
) -> (u16, Value) {
    let without_query = path.split('?').next().unwrap_or("");
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");

    if method == "GET" && without_query == "/api/v1/files" {
        // 两页：只有跟着游标再问一次的调用方才会看到第二页，这个循环值得测。
        let cursor = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("cursor="))
            .unwrap_or("");
        if cursor.is_empty() {
            return (
                200,
                json!({
                    "files": [catalog()[0].clone(), catalog()[1].clone()],
                    "pagination": { "listComplete": false, "cursor": "page-2" },
                }),
            );
        }
        return (
            200,
            json!({
                "files": [catalog()[2].clone()],
                "pagination": { "listComplete": true },
            }),
        );
    }

    // 站点删一个文件走的是单数路径 `DELETE /api/v1/file/<id>`（旧版本体实现也是这么发的），
    // 复数 `/api/v1/files/<id>` 在真站点上不存在。这里只认真实形状：万一哪天调用方改成复数，
    // 会掉到下面的 404 让用例当场红，而不是被宽容匹配收下、看着像删成功了。
    if method == "DELETE" && without_query.starts_with("/api/v1/file/") {
        let id = without_query
            .trim_start_matches("/api/v1/file/")
            .to_string();
        deleted.lock().unwrap().push(Delete { id });
        return (200, json!({ "ok": true }));
    }

    if method == "POST" && without_query == "/api/v1/upload" {
        let ctype = headers.get("content-type").cloned().unwrap_or_default();
        let mut item = parse_multipart(&ctype, body);
        item.path = path.to_string();
        item.content_type = ctype;
        item.idempotency_key = headers.get("idempotency-key").cloned().unwrap_or_default();
        received.lock().unwrap().push(item);
        return (
            200,
            json!({
                "file": { "name": "a.png" },
                "links": { "download": "/f/a.png" },
                "deduplicated": false,
            }),
        );
    }

    if method == "GET" && without_query.starts_with("/dav") {
        // DAV 这一面故意是「能连上但没用」：这个计数一旦上涨，就说明有什么东西
        // 拿 WebDAV 去读了插件托管的远程。
        return (200, json!({ "dav": "read-only stub" }));
    }

    (404, json!({ "message": "not found" }))
}

/// 最小 multipart 读取：够用来取回字段顺序、文件段和它的字节，不是通用解析器。
fn parse_multipart(content_type: &str, body: &[u8]) -> Received {
    let mut out = Received::default();
    let boundary = content_type
        .split(';')
        .find_map(|p| p.trim().strip_prefix("boundary="))
        .unwrap_or("")
        .trim_matches('"')
        .to_string();
    if boundary.is_empty() {
        return out;
    }
    let sep = format!("--{boundary}");
    let sep_bytes = sep.as_bytes();
    let mut offsets = Vec::new();
    let mut i = 0;
    while i + sep_bytes.len() <= body.len() {
        if &body[i..i + sep_bytes.len()] == sep_bytes {
            offsets.push(i);
            i += sep_bytes.len();
        } else {
            i += 1;
        }
    }
    for (n, start) in offsets.iter().enumerate() {
        let head_start = start + sep_bytes.len();
        let end = offsets.get(n + 1).copied().unwrap_or(body.len());
        if head_start >= end {
            continue;
        }
        let chunk = &body[head_start..end];
        let Some(split) = find_subslice(chunk, b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&chunk[..split]).to_string();
        let value_start = head_start + split + 4;
        let value_end = end.saturating_sub(2); // 下一段边界前的那个 CRLF
        let value = if value_start <= value_end {
            &body[value_start..value_end]
        } else {
            &[][..]
        };
        // 段头里 `name` 与文件段的 `filename` 都在 Content-Disposition 上；
        // 有 filename 就算文件段，其余按文本字段收。
        let name = attr(&head, "name").unwrap_or_default();
        let filename = attr(&head, "filename").unwrap_or_default();
        if !filename.is_empty() {
            out.file_field = name;
            out.file_name = filename;
            out.file_bytes = value.to_vec();
            out.file_offset = value_start;
        } else {
            if out.field_offset == 0 {
                out.field_offset = value_start;
            }
            out.fields
                .push((name, String::from_utf8_lossy(value).to_string()));
        }
    }
    out
}

/// 段头里 `key="..."` 的值。
fn attr(head: &str, key: &str) -> Option<String> {
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if !lower.starts_with("content-disposition:") {
            continue;
        }
        let needle = format!("{key}=\"");
        if let Some(pos) = lower.find(&needle) {
            let rest = &line[pos + needle.len()..];
            if let Some(end) = rest.find('"') {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}
