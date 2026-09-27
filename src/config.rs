//! Runtime configuration. The TOML file overrides the defaults, and flags and env vars
//! override the file.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

/// Where a value came from. Kept so the startup banner can tell the user what is in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    File,
    Env,
    Flag,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: IpAddr,
    pub port: u16,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub transcode_dir: PathBuf,
    /// Ceiling for the on disk cache. The evictor keeps the newest entries under it.
    pub cache_max_bytes: u64,
    /// Segment TTL for proxied HLS media. A segment is reused for this long.
    pub segment_ttl_secs: i64,
    /// Wall clock limit for one transcode job.
    pub transcode_timeout_secs: u64,
    /// Cut a conversion off after this many seconds of source. Zero means no limit.
    pub transcode_max_seconds: Option<u64>,
    pub hw_encode: bool,
    pub origins: Sources,
}

/// The upstream provider endpoints, kept in one place so a source swap is a config change.
#[derive(Debug, Clone)]
pub struct Sources {
    pub base: String,
    pub embed_servers: Vec<String>,
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            base: "https://hianime.at".to_string(),
            embed_servers: vec!["ZokoAnime".to_string()],
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8080,
            data_dir: default_data_dir(),
            cache_dir: PathBuf::new(),
            transcode_dir: PathBuf::new(),
            cache_max_bytes: 8 * 1024 * 1024 * 1024,
            segment_ttl_secs: 7 * 24 * 3600,
            transcode_timeout_secs: 4 * 3600,
            transcode_max_seconds: None,
            hw_encode: true,
            origins: Sources::default(),
        }
    }
}

fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("otakuhub");
        }
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => PathBuf::from(home).join(".local/share/otakuhub"),
        _ => PathBuf::from("otakuhub-data"),
    }
}

#[derive(Debug, Default)]
struct Overrides {
    bind: Option<String>,
    port: Option<u16>,
    data_dir: Option<String>,
    cache_max: Option<u64>,
    hw_encode: Option<bool>,
}

/// Parse `OTAKUHUB_` prefixed env vars. Only the keys below are honoured.
fn env_overrides() -> Overrides {
    let get = |k: &str| std::env::var(format!("OTAKUHUB_{k}")).ok().filter(|v| !v.is_empty());
    Overrides {
        bind: get("BIND"),
        port: get("PORT").and_then(|v| v.parse().ok()),
        data_dir: get("DATA_DIR"),
        cache_max: get("CACHE_MAX_BYTES").and_then(|v| v.parse().ok()),
        hw_encode: get("HW_ENCODE").map(|v| v == "1" || v.eq_ignore_ascii_case("true")),
    }
}

fn parse_args() -> Result<Overrides, String> {
    let mut o = Overrides::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut take = |name: &str| -> Result<String, String> {
            it.next().ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--bind" | "-b" => o.bind = Some(take("--bind")?),
            "--port" | "-p" => {
                let v = take("--port")?;
                o.port = Some(v.parse().map_err(|_| format!("bad port: {v}"))?);
            }
            "--data-dir" | "-d" => o.data_dir = Some(take("--data-dir")?),
            "--cache-max-bytes" => {
                let v = take("--cache-max-bytes")?;
                o.cache_max = Some(v.parse().map_err(|_| format!("bad size: {v}"))?);
            }
            "--software-encode" => o.hw_encode = Some(false),
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("otakuhub {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(o)
}

pub const USAGE: &str = "\
otakuhub, a local first anime provider

USAGE:
    otakuhub [OPTIONS]

OPTIONS:
    -b, --bind <ADDR>          listen address          [default: 127.0.0.1]
    -p, --port <PORT>          listen port             [default: 8080]
    -d, --data-dir <PATH>      state, db and cache     [default: $XDG_DATA_HOME/otakuhub]
        --cache-max-bytes <N>  cache ceiling in bytes   [default: 8589934592]
        --software-encode      ignore hardware encoders and use libx264/libx265
    -h, --help                 print this help
    -V, --version              print the version

ENV:
    OTAKUHUB_BIND, OTAKUHUB_PORT, OTAKUHUB_DATA_DIR,
    OTAKUHUB_CACHE_MAX_BYTES, OTAKUHUB_HW_ENCODE, OTAKUHUB_LOG";

impl Config {
    /// bind, port, cache_max_bytes and data_dir are always overwritten by the flag or the env
    /// var when either is set, so the TOML file is ignored for those four.
    pub fn load() -> Result<Self, String> {
        let flags = parse_args().map_err(|e| e.to_string())?;
        let env = env_overrides();
        let mut cfg = Config::default();

        let dir = flags
            .data_dir
            .clone()
            .or(env.data_dir.clone())
            .map(PathBuf::from)
            .unwrap_or_else(|| cfg.data_dir.clone());
        let file = read_file(&dir.join("otakuhub.toml"))?;
        if let Some(t) = &file {
            apply_toml(&mut cfg, t);
        }
        cfg.data_dir = dir;

        // The flag beats the env var here, as for every key except hw_encode.
        if let Some(v) = flags.bind.clone().or(env.bind.clone()) {
            cfg.bind = v
                .parse()
                .map_err(|_| format!("bad bind address: {v}"))?;
        }
        if let Some(p) = flags.port.or(env.port) {
            cfg.port = p;
        }
        if let Some(m) = flags.cache_max.or(env.cache_max) {
            cfg.cache_max_bytes = m;
        }
        if let Some(h) = env.hw_encode {
            cfg.hw_encode = h;
        }
        // Only a false flag is honoured, so OTAKUHUB_HW_ENCODE=0 beats `--hw-encode`.
        if flags.hw_encode == Some(false) {
            cfg.hw_encode = false;
        }

        cfg.cache_dir = cfg.data_dir.join("cache");
        cfg.transcode_dir = cfg.cache_dir.join("transcode");
        Ok(cfg)
    }

    /// Provenance of the settings an operator is most likely to get wrong.
    pub fn explain(&self) -> Vec<(&'static str, String)> {
        vec![
            ("bind", format!("{}:{}", self.bind, self.port)),
            ("data dir", self.data_dir.display().to_string()),
            ("cache", format!("{} (max {})", self.cache_dir.display(), human_bytes(self.cache_max_bytes))),
            ("hardware encode", if self.hw_encode { "on" } else { "off" }.to_string()),
            ("source", self.origins.base.clone()),
        ]
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("otakuhub.sqlite")
    }

    /// Absolute URL for this server, used for logging only.
    pub fn root_url(&self) -> String {
        format!("http://{}:{}", self.bind, self.port)
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Minimal TOML reader. Only flat `key = value` pairs.
fn read_file(path: &Path) -> Result<Option<TomlTable>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(parse_toml(&text)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[derive(Debug, Default)]
struct TomlTable {
    values: Vec<(String, TomlValue)>,
}

#[derive(Debug, Clone)]
enum TomlValue {
    Str(String),
    Num(f64),
    Bool(bool),
}

impl TomlTable {
    fn str(&self, k: &str) -> Option<String> {
        match self.values.iter().find(|(key, _)| key == k) {
            Some((_, TomlValue::Str(s))) => Some(s.clone()),
            _ => None,
        }
    }
    fn num(&self, k: &str) -> Option<f64> {
        match self.values.iter().find(|(key, _)| key == k) {
            Some((_, TomlValue::Num(n))) => Some(*n),
            _ => None,
        }
    }
    fn bool(&self, k: &str) -> Option<bool> {
        match self.values.iter().find(|(key, _)| key == k) {
            Some((_, TomlValue::Bool(b))) => Some(*b),
            _ => None,
        }
    }
}

fn parse_toml(text: &str) -> Result<TomlTable, String> {
    let mut table = TomlTable::default();
    let mut section = String::new();
    for (n, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            section = line
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_string();
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!("line {}: expected key = value", n + 1));
        };
        let key = if section.is_empty() {
            k.trim().to_string()
        } else {
            format!("{section}.{}", k.trim())
        };
        let val = v.trim();
        let parsed = if val.starts_with('"') {
            TomlValue::Str(val.trim_matches('"').to_string())
        } else if val == "true" {
            TomlValue::Bool(true)
        } else if val == "false" {
            TomlValue::Bool(false)
        } else {
            TomlValue::Num(
                val.parse()
                    .map_err(|_| format!("line {}: cannot read value {val}", n + 1))?,
            )
        };
        table.values.push((key, parsed));
    }
    Ok(table)
}

fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_str = false;
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_str = !in_str,
            b'#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

fn apply_toml(cfg: &mut Config, t: &TomlTable) {
    if let Some(v) = t.str("server.bind") {
        if let Ok(ip) = v.parse::<IpAddr>() {
            cfg.bind = ip;
        }
    }
    if let Some(v) = t.num("server.port") {
        cfg.port = v as u16;
    }
    if let Some(v) = t.num("cache.max_bytes") {
        cfg.cache_max_bytes = v as u64;
    }
    if let Some(v) = t.num("cache.segment_ttl_secs") {
        cfg.segment_ttl_secs = v as i64;
    }
    if let Some(v) = t.bool("transcode.hardware") {
        cfg.hw_encode = v;
    }
    if let Some(v) = t.num("transcode.timeout_secs") {
        cfg.transcode_timeout_secs = v as u64;
    }
    if let Some(v) = t.num("transcode.max_seconds") {
        cfg.transcode_max_seconds = (v > 0.0).then_some(v as u64);
    }
    if let Some(v) = t.str("source.base") {
        cfg.origins.base = v.trim_end_matches('/').to_string();
    }
    if let Some(v) = t.str("source.embed_servers") {
        cfg.origins.embed_servers =
            v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_toml_with_sections() {
        let t = parse_toml(
            r#"
            # a comment
            [server]
            port = 9000
            bind = "0.0.0.0"

            [cache]
            max_bytes = 1024
            "#,
        )
        .unwrap();
        assert_eq!(t.num("server.port"), Some(9000.0));
        assert_eq!(t.str("server.bind").as_deref(), Some("0.0.0.0"));
        assert_eq!(t.num("cache.max_bytes"), Some(1024.0));
    }

    #[test]
    fn comment_inside_a_string_is_kept() {
        let t = parse_toml(r#"key = "a#b" "#).unwrap();
        assert_eq!(t.str("key").as_deref(), Some("a#b"));
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(8 * 1024 * 1024 * 1024), "8.0 GiB");
    }

    #[test]
    fn toml_overrides_defaults() {
        let mut cfg = Config::default();
        let t = parse_toml("[server]\nport = 1234\n[transcode]\nhardware = false\n").unwrap();
        apply_toml(&mut cfg, &t);
        assert_eq!(cfg.port, 1234);
        assert!(!cfg.hw_encode);
    }

    #[test]
    fn bind_rejects_garbage() {
        let t = parse_toml("[server]\nbind = \"not-an-ip\"\n").unwrap();
        let mut cfg = Config::default();
        apply_toml(&mut cfg, &t);
        assert_eq!(cfg.bind, IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
}
