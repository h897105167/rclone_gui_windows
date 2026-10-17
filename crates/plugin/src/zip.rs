//! ZIP 读取与解压（最小实现，仅支持"读取"场景）。
//!
//! 为什么不用 `zip` crate：本仓库在离线环境构建，`zip` 及其依赖不在本地
//! registry 中；而 `flate2`（deflate 解压）已经可用。插件包只需要"解开别人
//! 打好的 zip"，不需要写入、加密、多卷等能力，因此这里直接按 ZIP 规范实现
//! 读取路径，行为可控且无额外依赖。
//!
//! 支持：store(0) 与 deflate(8) 两种压缩方法、ZIP64 扩展、UTF-8 文件名。
//! 不支持：加密、多卷、以及压缩方法 8/0 之外的条目（遇到即报错，不静默跳过）。

use anyhow::{anyhow, bail, Context, Result};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

/// ZIP 单个条目
#[derive(Debug, Clone)]
pub struct ZipEntry {
    /// 归档内的相对路径（已统一为 '/' 分隔，且已做安全校验）
    pub name: String,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub method: u16,
    /// 该条目数据在文件中的起始偏移
    pub data_offset: u64,
    pub is_dir: bool,
    /// 中央目录记录的 CRC-32
    pub crc32: u32,
}

const EOCD_SIG: u32 = 0x0605_4b50;
const EOCD64_LOCATOR_SIG: u32 = 0x0706_4b50;
const EOCD64_SIG: u32 = 0x0606_4b50;
const CEN_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;

const METHOD_STORE: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

/// 读取 zip 的中央目录。
pub fn read_entries(data: &[u8]) -> Result<Vec<ZipEntry>> {
    if data.len() < 22 {
        bail!("文件太小，不是有效的 zip");
    }
    let eocd_off = find_eocd(data).ok_or_else(|| anyhow!("未找到 zip 结尾标记（EOCD），文件可能已损坏"))?;
    let mut cen_count = u16::from_le_bytes([data[eocd_off + 10], data[eocd_off + 11]]) as u64;
    let mut cen_off = u32::from_le_bytes([
        data[eocd_off + 16],
        data[eocd_off + 17],
        data[eocd_off + 18],
        data[eocd_off + 19],
    ]) as u64;

    // ZIP64：EOCD 中对应字段为 0xFFFF/0xFFFFFFFF 时，去 ZIP64 EOCD 取真实值
    if cen_count == 0xFFFF
        || cen_off == 0xFFFF_FFFF
        || eocd_off >= 20 && u32::from_le_bytes([data[eocd_off - 20], data[eocd_off - 19], data[eocd_off - 18], data[eocd_off - 17]]) == EOCD64_LOCATOR_SIG
    {
        if let Some(z64) = find_zip64_eocd(data, eocd_off) {
            cen_count = u64::from_le_bytes(data[z64 + 32..z64 + 40].try_into().unwrap());
            cen_off = u64::from_le_bytes(data[z64 + 48..z64 + 56].try_into().unwrap());
        }
    }

    let mut entries = Vec::new();
    let mut p = cen_off as usize;
    for _ in 0..cen_count {
        if p + 46 > data.len() {
            bail!("zip 中央目录越界，文件可能已损坏");
        }
        let sig = u32::from_le_bytes(data[p..p + 4].try_into().unwrap());
        if sig != CEN_SIG {
            bail!("zip 中央目录签名错误（偏移 {p}）");
        }
        let method = u16::from_le_bytes([data[p + 10], data[p + 11]]);
        let crc32 = u32::from_le_bytes(data[p + 16..p + 20].try_into().unwrap());
        let mut csize = u32::from_le_bytes(data[p + 20..p + 24].try_into().unwrap()) as u64;
        let mut usize_ = u32::from_le_bytes(data[p + 24..p + 28].try_into().unwrap()) as u64;
        let name_len = u16::from_le_bytes([data[p + 28], data[p + 29]]) as usize;
        let extra_len = u16::from_le_bytes([data[p + 30], data[p + 31]]) as usize;
        let comment_len = u16::from_le_bytes([data[p + 32], data[p + 33]]) as usize;
        let mut local_off = u32::from_le_bytes(data[p + 42..p + 46].try_into().unwrap()) as u64;

        if p + 46 + name_len > data.len() {
            bail!("zip 条目名越界，文件可能已损坏");
        }
        let raw_name = &data[p + 46..p + 46 + name_len];
        // 注意：部分打包工具（含 Windows 自带的 Compress-Archive）在归档名里
        // 使用反斜杠分隔符，与 ZIP 规范要求的 '/' 不符。这里统一规范化为 '/',
        // 否则按 '/' 匹配路径（如 plugin.json、manifest.sha256）会全部失效。
        let name = decode_name(raw_name).replace('\\', "/");

        // ZIP64 扩展字段：补齐 0xFFFFFFFF 的字段
        let extra = &data[p + 46 + name_len..(p + 46 + name_len + extra_len).min(data.len())];
        if csize == 0xFFFF_FFFF || usize_ == 0xFFFF_FFFF || local_off == 0xFFFF_FFFF {
            let (c, u, o) = parse_zip64_extra(extra, csize, usize_, local_off);
            csize = c;
            usize_ = u;
            local_off = o;
        }

        // 本地头长度可能比中央目录记录的 extra 更长，需按本地头重新定位数据起点
        let lo = local_off as usize;
        if lo + 30 > data.len() {
            bail!("zip 本地头越界（条目 {name}）");
        }
        if u32::from_le_bytes(data[lo..lo + 4].try_into().unwrap()) != LOCAL_SIG {
            bail!("zip 本地头签名错误（条目 {name}）");
        }
        let l_name_len = u16::from_le_bytes([data[lo + 26], data[lo + 27]]) as usize;
        let l_extra_len = u16::from_le_bytes([data[lo + 28], data[lo + 29]]) as usize;
        let data_offset = (lo + 30 + l_name_len + l_extra_len) as u64;

        entries.push(ZipEntry {
            is_dir: name.ends_with('/'),
            name,
            compressed_size: csize,
            uncompressed_size: usize_,
            method,
            data_offset,
            crc32,
        });
        p += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

/// 从 EOCD 往前找（EOCD 注释最长 65535 字节）
fn find_eocd(data: &[u8]) -> Option<usize> {
    let start = data.len().saturating_sub(22 + 65535);
    let mut i = data.len().checked_sub(22)?;
    loop {
        if i + 4 <= data.len() && u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) == EOCD_SIG {
            return Some(i);
        }
        if i == start {
            return None;
        }
        i -= 1;
    }
}

fn find_zip64_eocd(data: &[u8], eocd_off: usize) -> Option<usize> {
    // locator 紧邻 EOCD 之前，20 字节
    if eocd_off < 20 {
        return None;
    }
    let loc = eocd_off - 20;
    if u32::from_le_bytes(data[loc..loc + 4].try_into().unwrap()) != EOCD64_LOCATOR_SIG {
        return None;
    }
    let z64 = u64::from_le_bytes(data[loc + 8..loc + 16].try_into().unwrap()) as usize;
    if z64 + 56 > data.len() {
        return None;
    }
    if u32::from_le_bytes(data[z64..z64 + 4].try_into().unwrap()) != EOCD64_SIG {
        return None;
    }
    Some(z64)
}

/// 解析 ZIP64 扩展字段，替换 0xFFFFFFFF 占位值
fn parse_zip64_extra(extra: &[u8], csize: u64, usize_: u64, local_off: u64) -> (u64, u64, u64) {
    let mut c = csize;
    let mut u = usize_;
    let mut o = local_off;
    let mut i = 0usize;
    while i + 4 <= extra.len() {
        let tag = u16::from_le_bytes([extra[i], extra[i + 1]]);
        let len = u16::from_le_bytes([extra[i + 2], extra[i + 3]]) as usize;
        let body = &extra[i + 4..(i + 4 + len).min(extra.len())];
        if tag == 0x0001 {
            let mut q = 0usize;
            let mut next = |need: usize| -> Option<u64> {
                if q + need <= body.len() {
                    let v = u64::from_le_bytes(body[q..q + need].try_into().ok()?);
                    q += need;
                    Some(v)
                } else {
                    None
                }
            };
            if usize_ == 0xFFFF_FFFF {
                if let Some(v) = next(8) {
                    u = v;
                }
            }
            if csize == 0xFFFF_FFFF {
                if let Some(v) = next(8) {
                    c = v;
                }
            }
            if local_off == 0xFFFF_FFFF {
                if let Some(v) = next(8) {
                    o = v;
                }
            }
        }
        i += 4 + len;
    }
    (c, u, o)
}

/// 文件名解码：ZIP 规范里非 UTF-8 时通常是 CP437。这里优先 UTF-8，
/// 失败则按字节保留（插件包内文件名基本都是 ASCII）。
fn decode_name(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => raw.iter().map(|&b| b as char).collect(),
    }
}

/// 解出单个条目的内容。
pub fn read_entry(data: &[u8], e: &ZipEntry) -> Result<Vec<u8>> {
    if e.is_dir {
        return Ok(Vec::new());
    }
    let start = e.data_offset as usize;
    let end = start
        .checked_add(e.compressed_size as usize)
        .ok_or_else(|| anyhow!("条目大小溢出: {}", e.name))?;
    if end > data.len() {
        bail!("zip 数据越界（条目 {}）", e.name);
    }
    let raw = &data[start..end];
    let out = match e.method {
        METHOD_STORE => raw.to_vec(),
        METHOD_DEFLATE => {
            let mut out = Vec::with_capacity(e.uncompressed_size as usize);
            flate2::read::DeflateDecoder::new(Cursor::new(raw))
                .read_to_end(&mut out)
                .with_context(|| format!("解压失败（条目 {}）", e.name))?;
            out
        }
        m => bail!("不支持的 zip 压缩方法 {m}（条目 {}）", e.name),
    };
    // 校验 CRC-32：能发现 zip 在传输/下载中损坏，避免把坏文件当插件装上
    if e.crc32 != 0 || !out.is_empty() {
        let mut h = crc32fast::Hasher::new();
        h.update(&out);
        let got = h.finalize();
        if got != e.crc32 {
            bail!(
                "zip 条目校验失败（{}）：CRC-32 不匹配，文件可能已损坏",
                e.name
            );
        }
    }
    Ok(out)
}

/// 路径安全校验：拒绝绝对路径、盘符、以及任何 `..` 上跳，
/// 防止恶意 zip 覆盖插件目录之外的文件（zip slip）。
pub fn safe_join(root: &Path, name: &str) -> Result<PathBuf> {
    let rel = Path::new(name);
    let mut out = root.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => bail!("插件包内含非法路径（..）: {name}"),
            Component::RootDir | Component::Prefix(_) => bail!("插件包内含绝对路径: {name}"),
        }
    }
    Ok(out)
}

/// 把 zip 全部解压到 `dest`。返回解出的文件数。
///
/// 限制总解压体积，避免"zip 炸弹"耗尽磁盘（插件包本身不该这么大）。
pub fn extract_all(data: &[u8], dest: &Path, max_total: u64) -> Result<usize> {
    let entries = read_entries(data)?;
    let mut total: u64 = 0;
    let mut files = 0usize;
    for e in &entries {
        total = total.saturating_add(e.uncompressed_size);
        if total > max_total {
            bail!(
                "解压后体积超过上限（{} MB），已中止",
                max_total / 1024 / 1024
            );
        }
        let path = safe_join(dest, &e.name)?;
        if e.is_dir {
            std::fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = read_entry(data, e)?;
        let mut f = std::fs::File::create(&path)
            .with_context(|| format!("写入失败: {}", path.display()))?;
        f.write_all(&content)?;
        files += 1;
    }
    Ok(files)
}

/// 读取 zip 内单个文件的内容（不解压全部）。找不到返回 None。
pub fn read_file_by_name(data: &[u8], want: &str) -> Result<Option<Vec<u8>>> {
    let want_norm = want.replace('\\', "/");
    for e in read_entries(data)? {
        // 允许插件包把文件放在单层顶层目录里（很多打包工具会这样）
        let n = e.name.trim_start_matches("./");
        if n == want_norm || n.ends_with(&format!("/{want_norm}")) {
            return Ok(Some(read_entry(data, &e)?));
        }
    }
    Ok(None)
}

/// 在 zip 中查找 plugin.json，返回 (归档内路径, 内容)。
/// 支持文件位于顶层或单层子目录（"winfsp/plugin.json"）。
pub fn find_plugin_json(data: &[u8]) -> Result<Option<(String, Vec<u8>)>> {
    for e in read_entries(data)? {
        let n = e.name.trim_start_matches("./").to_string();
        if n == "plugin.json" || n.ends_with("/plugin.json") {
            // 只接受最多一层子目录，避免误取深层无关文件
            if n.matches('/').count() <= 1 {
                return Ok(Some((n, read_entry(data, &e)?)));
            }
        }
    }
    Ok(None)
}

/// 计算 sha256（小写十六进制）
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    to_hex(&h.finalize())
}

/// 计算文件 sha256
pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("打开文件失败: {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(to_hex(&h.finalize()))
}

fn to_hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 0xf) as usize] as char);
    }
    s
}

/// 供其它模块复用的 Seek 包装（当前仅测试使用，保留以便扩展）
#[allow(dead_code)]
pub(crate) fn cursor(data: &[u8]) -> Cursor<&[u8]> {
    let mut c = Cursor::new(data);
    let _ = c.seek(SeekFrom::Start(0));
    c
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// 构造一个 store 模式的 zip（多文件）。仅供测试使用。
    pub fn build_store_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        let mut count = 0u16;
        for (name, content) in files {
            let crc = crc32(content);
            let local_off = out.len() as u32;
            out.extend_from_slice(&LOCAL_SIG.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&METHOD_STORE.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(content.len() as u32).to_le_bytes());
            out.extend_from_slice(&(content.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(content);

            central.extend_from_slice(&CEN_SIG.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&METHOD_STORE.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(content.len() as u32).to_le_bytes());
            central.extend_from_slice(&(content.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&local_off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
            count += 1;
        }
        let cen_off = out.len() as u32;
        let cen_size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&EOCD_SIG.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&cen_size.to_le_bytes());
        out.extend_from_slice(&cen_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// 构造一个 store 模式的最小 zip（单文件）
    fn make_store_zip(name: &str, content: &[u8]) -> Vec<u8> {
        build_store_zip(&[(name, content)])
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut h = crc32fast::Hasher::new();
        h.update(data);
        h.finalize()
    }

    /// 构造 deflate 模式的 zip（真实 zip 工具默认用 deflate）
    fn make_deflate_zip(name: &str, content: &[u8]) -> Vec<u8> {
        use flate2::write::DeflateEncoder;
        use flate2::Compression;
        use std::io::Write as _;
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(content).unwrap();
        let comp = enc.finish().unwrap();

        let crc = crc32(content);
        let mut out = Vec::new();
        out.extend_from_slice(&LOCAL_SIG.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&METHOD_DEFLATE.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&comp);
        let cen_off = out.len();
        out.extend_from_slice(&CEN_SIG.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&METHOD_DEFLATE.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        let cen_size = out.len() - cen_off;
        out.extend_from_slice(&EOCD_SIG.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(cen_size as u32).to_le_bytes());
        out.extend_from_slice(&(cen_off as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// 反斜杠分隔符必须被规范化：Windows 自带的 Compress-Archive 就产出
    /// `dir\plugin.json` 这种名字，若只按 '/' 匹配会完全找不到插件元数据。
    #[test]
    fn normalizes_backslash_separators() {
        let zip = make_store_zip("winfsp\\plugin.json", b"{\"id\":\"a\"}");
        let es = read_entries(&zip).unwrap();
        assert_eq!(es[0].name, "winfsp/plugin.json", "应把 \\ 规范化为 /");
        let (n, _) = find_plugin_json(&zip).unwrap().unwrap();
        assert_eq!(n, "winfsp/plugin.json");
    }

    /// deflate 条目能正确解压（真实 zip 默认压缩方式）
    #[test]
    fn reads_deflate_entry() {
        let payload = b"hello deflate world, hello deflate world".repeat(20);
        let zip = make_deflate_zip("plugin.json", &payload);
        let es = read_entries(&zip).unwrap();
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].method, METHOD_DEFLATE);
        assert!(es[0].compressed_size < es[0].uncompressed_size, "应确实被压缩");
        assert_eq!(read_entry(&zip, &es[0]).unwrap(), payload);
    }

    /// CRC 不匹配必须被发现（损坏的 zip 不能当插件装上）
    #[test]
    fn detects_corrupted_entry() {
        let zip = make_store_zip("a.txt", b"original");
        // 篡改内容但保留原 CRC
        let mut bad = zip.clone();
        let es = read_entries(&bad).unwrap();
        let off = es[0].data_offset as usize;
        bad[off] = b'X';
        let err = read_entry(&bad, &es[0]).unwrap_err().to_string();
        assert!(err.contains("CRC-32"), "应报 CRC 校验失败，实际: {err}");
    }

    #[test]
    fn reads_store_entry() {
        let zip = make_store_zip("hello.txt", b"world");
        let es = read_entries(&zip).unwrap();
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].name, "hello.txt");
        assert_eq!(read_entry(&zip, &es[0]).unwrap(), b"world");
    }

    #[test]
    fn finds_plugin_json_at_root_and_in_subdir() {
        let z1 = make_store_zip("plugin.json", b"{\"id\":\"a\"}");
        let (n, c) = find_plugin_json(&z1).unwrap().unwrap();
        assert_eq!(n, "plugin.json");
        assert_eq!(c, b"{\"id\":\"a\"}");

        let z2 = make_store_zip("winfsp/plugin.json", b"{\"id\":\"b\"}");
        let (n2, _) = find_plugin_json(&z2).unwrap().unwrap();
        assert_eq!(n2, "winfsp/plugin.json");
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(safe_join(Path::new("C:/dest"), "../evil.txt").is_err());
        assert!(safe_join(Path::new("C:/dest"), "a/../../evil.txt").is_err());
        assert!(safe_join(Path::new("C:/dest"), "C:/windows/system32/x").is_err());
        assert!(safe_join(Path::new("C:/dest"), "a/b.txt").is_ok());
    }

    #[test]
    fn sha256_matches_known_vector() {
        // 空输入的 sha256 是众所周知的常量
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn reads_zip64_placeholder_fields() {
        // 普通小文件不触发 ZIP64，确认解析路径正常
        let zip = make_store_zip("a.bin", &vec![7u8; 1000]);
        let es = read_entries(&zip).unwrap();
        assert_eq!(es[0].uncompressed_size, 1000);
        assert_eq!(read_entry(&zip, &es[0]).unwrap().len(), 1000);
    }
}
