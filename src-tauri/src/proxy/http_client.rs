//! 全局 HTTP 客户端模块
//!
//! 提供支持全局代理配置的 HTTP 客户端。
//! 所有需要发送 HTTP 请求的模块都应使用此模块提供的客户端。

use once_cell::sync::OnceCell;
use reqwest::Client;
use std::env;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::Duration;

/// 全局 HTTP 客户端实例
static GLOBAL_CLIENT: OnceCell<RwLock<Client>> = OnceCell::new();

/// 当前代理 URL（用于日志和状态查询）
static CURRENT_PROXY_URL: OnceCell<RwLock<Option<String>>> = OnceCell::new();

/// CC Switch 代理服务器当前监听的端口
static CC_SWITCH_PROXY_PORT: OnceCell<RwLock<u16>> = OnceCell::new();

/// 当前全局客户端所采用的"跟随系统代理"结果（None = 直连）。
/// 显式用户代理生效时该值无意义（刷新逻辑会让位于用户设置）。
static BAKED_SYSTEM_PROXY: OnceCell<RwLock<Option<String>>> = OnceCell::new();

/// 客户端因系统代理变化而重建的次数（诊断与回归测试用）
static SYSTEM_PROXY_REBUILD_COUNT: AtomicU64 = AtomicU64::new(0);

/// 上次检查系统代理变化的时间戳（毫秒），用于节流
static LAST_SYSTEM_PROXY_CHECK_MS: AtomicU64 = AtomicU64::new(0);

/// 跟随系统代理的变化检查间隔（毫秒）
const SYSTEM_PROXY_RECHECK_INTERVAL_MS: u64 = 5000;

/// 设置 CC Switch 代理服务器的监听端口
///
/// 应在代理服务器启动时调用，以便系统代理检测能正确识别自己的端口
pub fn set_proxy_port(port: u16) {
    if let Some(lock) = CC_SWITCH_PROXY_PORT.get() {
        if let Ok(mut current_port) = lock.write() {
            *current_port = port;
            log::debug!("[GlobalProxy] Updated CC Switch proxy port to {port}");
        }
    } else {
        let _ = CC_SWITCH_PROXY_PORT.set(RwLock::new(port));
        log::debug!("[GlobalProxy] Initialized CC Switch proxy port to {port}");
    }
}

/// 获取 CC Switch 代理服务器的监听端口
fn get_proxy_port() -> u16 {
    CC_SWITCH_PROXY_PORT
        .get()
        .and_then(|lock| lock.read().ok())
        .map(|port| *port)
        .unwrap_or(15721) // 默认端口作为回退
}

/// 初始化全局 HTTP 客户端
///
/// 应在应用启动时调用一次。
///
/// # Arguments
/// * `proxy_url` - 代理 URL，如 `http://127.0.0.1:7890` 或 `socks5://127.0.0.1:1080`
///   传入 None 或空字符串表示直连
pub fn init(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    let client = build_client(effective_url)?;

    // 尝试初始化全局客户端，如果已存在则记录警告并使用 apply_proxy 更新
    if GLOBAL_CLIENT.set(RwLock::new(client.clone())).is_err() {
        log::warn!(
            "[GlobalProxy] [GP-003] Already initialized, updating instead: {}",
            effective_url
                .map(mask_url)
                .unwrap_or_else(|| "direct connection".to_string())
        );
        // 已初始化，改用 apply_proxy 更新
        return apply_proxy(proxy_url);
    }

    // 初始化代理 URL 记录
    let _ = CURRENT_PROXY_URL.set(RwLock::new(effective_url.map(|s| s.to_string())));
    record_baked_system_proxy(effective_url);

    log::info!(
        "[GlobalProxy] Initialized: {}",
        effective_url
            .map(mask_url)
            .unwrap_or_else(|| "direct connection".to_string())
    );

    Ok(())
}

/// 验证代理配置（不应用）
///
/// 只验证代理 URL 是否有效，不实际更新全局客户端。
/// 用于在持久化之前验证配置的有效性。
///
/// # Arguments
/// * `proxy_url` - 代理 URL，None 或空字符串表示直连
///
/// # Returns
/// 验证成功返回 Ok(())，失败返回错误信息
pub fn validate_proxy(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    // 只调用 build_client 来验证，但不应用
    build_client(effective_url)?;
    Ok(())
}

/// 应用代理配置（假设已验证）
///
/// 直接应用代理配置到全局客户端，不做额外验证。
/// 应在 validate_proxy 成功后调用。
///
/// # Arguments
/// * `proxy_url` - 代理 URL，None 或空字符串表示直连
pub fn apply_proxy(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    let new_client = build_client(effective_url)?;

    // 更新客户端
    if let Some(lock) = GLOBAL_CLIENT.get() {
        let mut client = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-001] Failed to acquire write lock: {e}");
            "Failed to update proxy: lock poisoned".to_string()
        })?;
        *client = new_client;
    } else {
        // 如果还没初始化，则初始化
        return init(proxy_url);
    }

    // 更新代理 URL 记录
    if let Some(lock) = CURRENT_PROXY_URL.get() {
        let mut url = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-002] Failed to acquire URL write lock: {e}");
            "Failed to update proxy URL record: lock poisoned".to_string()
        })?;
        *url = effective_url.map(|s| s.to_string());
    }
    record_baked_system_proxy(effective_url);

    log::info!(
        "[GlobalProxy] Applied: {}",
        effective_url
            .map(mask_url)
            .unwrap_or_else(|| "direct connection".to_string())
    );

    Ok(())
}

/// 更新代理配置（热更新）
///
/// 可在运行时调用以更改代理设置，无需重启应用。
/// 注意：此函数同时验证和应用，如果需要先验证后持久化再应用，
/// 请使用 validate_proxy + apply_proxy 组合。
///
/// # Arguments
/// * `proxy_url` - 新的代理 URL，None 或空字符串表示直连
#[allow(dead_code)]
pub fn update_proxy(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    let new_client = build_client(effective_url)?;

    // 更新客户端
    if let Some(lock) = GLOBAL_CLIENT.get() {
        let mut client = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-001] Failed to acquire write lock: {e}");
            "Failed to update proxy: lock poisoned".to_string()
        })?;
        *client = new_client;
    } else {
        // 如果还没初始化，则初始化
        return init(proxy_url);
    }

    // 更新代理 URL 记录
    if let Some(lock) = CURRENT_PROXY_URL.get() {
        let mut url = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-002] Failed to acquire URL write lock: {e}");
            "Failed to update proxy URL record: lock poisoned".to_string()
        })?;
        *url = effective_url.map(|s| s.to_string());
    }
    record_baked_system_proxy(effective_url);

    log::info!(
        "[GlobalProxy] Updated: {}",
        effective_url
            .map(mask_url)
            .unwrap_or_else(|| "direct connection".to_string())
    );

    Ok(())
}

/// 获取全局 HTTP 客户端
///
/// 返回配置了代理的客户端（如果已配置代理），否则返回跟随系统代理的客户端。
/// 未配置显式代理时（macOS），会节流地检测系统代理变化并按需重建客户端，
/// 避免应用启动后系统代理关闭/切换导致请求持续打到失效代理上。
pub fn get() -> Client {
    // 先做节流的变化检测再取客户端：若系统代理已变化，本次调用就直接拿到
    // 重建后的客户端，而不是把旧客户端（可能仍指向失效代理）多返回一次。
    refresh_system_proxy_if_changed();

    GLOBAL_CLIENT
        .get()
        .and_then(|lock| lock.read().ok())
        .map(|c| c.clone())
        .unwrap_or_else(|| {
            log::warn!("[GlobalProxy] [GP-004] Client not initialized, using fallback");
            build_client(None).unwrap_or_default()
        })
}

/// 写入"当前客户端所采用的系统代理解析结果"快照（未初始化则初始化）
fn store_baked_system_proxy(baked: Option<String>) {
    match BAKED_SYSTEM_PROXY.get() {
        Some(lock) => {
            if let Ok(mut baked_lock) = lock.write() {
                *baked_lock = baked;
            }
        }
        None => {
            let _ = BAKED_SYSTEM_PROXY.set(RwLock::new(baked));
        }
    }
}

/// 记录当前全局客户端所采用的系统代理解析结果
///
/// 显式用户代理时无需记录（刷新逻辑让位于用户设置），传 `Some(_)` 即可。
fn record_baked_system_proxy(explicit_url: Option<&str>) {
    let baked = if explicit_url.is_some() {
        None
    } else {
        current_effective_system_proxy()
    };
    store_baked_system_proxy(baked);
}

/// 节流地检查"跟随系统代理"的解析结果是否变化，变化则重建全局客户端
///
/// 仅在未配置显式代理时生效；显式代理由用户管理，不参与自动刷新。
/// 平台边界：变化检测只服务 macOS 的死本机代理旁路（见 [`build_client`]），
/// 其余平台维持原有行为，不做周期性解析。
fn refresh_system_proxy_if_changed() {
    if !cfg!(target_os = "macos") {
        return;
    }

    // 显式用户代理生效时，跳过自动刷新
    if get_current_proxy_url().is_some() {
        return;
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_SYSTEM_PROXY_CHECK_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) < SYSTEM_PROXY_RECHECK_INTERVAL_MS {
        return;
    }
    LAST_SYSTEM_PROXY_CHECK_MS.store(now_ms, Ordering::Relaxed);

    let current = current_effective_system_proxy();
    let baked = BAKED_SYSTEM_PROXY
        .get()
        .and_then(|lock| lock.read().ok())
        .and_then(|b| b.clone());
    if current == baked {
        return;
    }

    // 系统代理状态发生变化（开启/关闭/换端口/存活状态翻转），重建客户端
    match build_client(None) {
        Ok(new_client) => {
            if let Some(lock) = GLOBAL_CLIENT.get() {
                if let Ok(mut client) = lock.write() {
                    *client = new_client;
                    store_baked_system_proxy(current.clone());
                    SYSTEM_PROXY_REBUILD_COUNT.fetch_add(1, Ordering::Relaxed);
                    log::info!(
                        "[GlobalProxy] System proxy changed, rebuilt client: {}",
                        current
                            .as_deref()
                            .map(mask_url)
                            .unwrap_or_else(|| "direct connection".to_string())
                    );
                }
            }
        }
        Err(e) => {
            log::warn!("[GlobalProxy] Failed to rebuild client after system proxy change: {e}")
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
fn system_proxy_rebuild_count() -> u64 {
    SYSTEM_PROXY_REBUILD_COUNT.load(Ordering::Relaxed)
}

#[cfg(all(test, target_os = "macos"))]
fn reset_system_proxy_check_throttle() {
    LAST_SYSTEM_PROXY_CHECK_MS.store(0, Ordering::Relaxed);
}

/// 原始解析"跟随系统代理"的目标：环境变量 → macOS 系统代理（不做存活判定）
fn raw_system_proxy_url() -> Option<String> {
    env_system_proxy_url()
        .or_else(macos_system_proxy_url)
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
}

/// 解析当前"跟随系统代理"的有效结果（已过滤失效的本机代理）
///
/// 在 [`raw_system_proxy_url`] 基础上做存活判定：解析结果若指向本机 loopback
/// 且端口无进程监听（代理工具已退出/切换为 TUN 模式），视为无代理（直连），
/// 避免客户端把请求持续发往失效代理导致所有查询瞬间失败。该结果同时用作
/// 变化检测的签名（见 [`refresh_system_proxy_if_changed`]）。
fn current_effective_system_proxy() -> Option<String> {
    raw_system_proxy_url()
        .filter(|url| !proxy_url_is_loopback(url) || loopback_proxy_listening(url))
}

/// 从环境变量解析代理 URL（与 hyper-util 的优先级对齐：HTTPS > HTTP > ALL）
fn env_system_proxy_url() -> Option<String> {
    const KEY_GROUPS: [&[&str]; 3] = [
        &["HTTPS_PROXY", "https_proxy"],
        &["HTTP_PROXY", "http_proxy"],
        &["ALL_PROXY", "all_proxy"],
    ];
    KEY_GROUPS.iter().find_map(|keys| {
        keys.iter().find_map(|key| {
            env::var(key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
    })
}

/// 读取 macOS 系统代理（SCDynamicStore，与 reqwest/hyper-util 的数据源一致）
#[cfg(target_os = "macos")]
fn macos_system_proxy_url() -> Option<String> {
    use system_configuration::core_foundation::base::CFType;
    use system_configuration::core_foundation::dictionary::CFDictionary;
    use system_configuration::core_foundation::number::CFNumber;
    use system_configuration::core_foundation::string::{CFString, CFStringRef};
    use system_configuration::dynamic_store::SCDynamicStoreBuilder;
    use system_configuration::sys::schema_definitions::{
        kSCPropNetProxiesHTTPEnable, kSCPropNetProxiesHTTPPort, kSCPropNetProxiesHTTPProxy,
        kSCPropNetProxiesHTTPSEnable, kSCPropNetProxiesHTTPSPort, kSCPropNetProxiesHTTPSProxy,
    };

    fn read_entry(
        proxies_map: &CFDictionary<CFString, CFType>,
        enabled_key: CFStringRef,
        host_key: CFStringRef,
        port_key: CFStringRef,
    ) -> Option<String> {
        let enabled = proxies_map
            .find(enabled_key)
            .and_then(|flag| flag.downcast::<CFNumber>())
            .and_then(|flag| flag.to_i32())
            .unwrap_or(0)
            == 1;
        if !enabled {
            return None;
        }
        let host = proxies_map
            .find(host_key)
            .and_then(|v| v.downcast::<CFString>())
            .map(|v| v.to_string())?;
        let port = proxies_map
            .find(port_key)
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|v| v.to_i32())?;
        Some(format!("http://{host}:{port}"))
    }

    let store = SCDynamicStoreBuilder::new("cc-switch").build()?;
    let proxies_map = store.get_proxies()?;
    // HTTPS 代理优先：用量查询目标几乎全是 https 端点
    read_entry(
        &proxies_map,
        unsafe { kSCPropNetProxiesHTTPSEnable },
        unsafe { kSCPropNetProxiesHTTPSProxy },
        unsafe { kSCPropNetProxiesHTTPSPort },
    )
    .or_else(|| {
        read_entry(
            &proxies_map,
            unsafe { kSCPropNetProxiesHTTPEnable },
            unsafe { kSCPropNetProxiesHTTPProxy },
            unsafe { kSCPropNetProxiesHTTPPort },
        )
    })
}

#[cfg(not(target_os = "macos"))]
fn macos_system_proxy_url() -> Option<String> {
    None
}

/// 判断代理 URL 是否指向本机 loopback 地址
fn proxy_url_is_loopback(url: &str) -> bool {
    let parsed = url::Url::parse(url)
        .ok()
        .or_else(|| url::Url::parse(&format!("http://{url}")).ok());
    parsed
        .as_ref()
        .and_then(|parsed| parsed.host_str())
        .map(host_is_loopback)
        .unwrap_or(false)
}

/// 探测 loopback 代理端口是否有进程监听
///
/// 仅对 loopback 地址做探测（连接为微秒级）；非 loopback 地址一律视为可用。
fn loopback_proxy_listening(url: &str) -> bool {
    use std::net::TcpStream;

    let parsed = url::Url::parse(url)
        .ok()
        .or_else(|| url::Url::parse(&format!("http://{url}")).ok());
    let Some(parsed) = parsed else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if !host_is_loopback(host) {
        return true;
    }
    let Some(port) = parsed.port_or_known_default() else {
        return false;
    };

    use std::net::ToSocketAddrs;
    let addrs = match (host, port).to_socket_addrs() {
        Ok(addrs) => addrs.collect::<Vec<_>>(),
        Err(_) => return false,
    };
    addrs
        .iter()
        .any(|addr| TcpStream::connect_timeout(addr, Duration::from_millis(300)).is_ok())
}

fn host_is_loopback(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // url crate 对 IPv6 host 保留方括号（如 "[::1]"），解析前需剥掉
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// 获取当前代理 URL
///
/// 返回当前配置的代理 URL，None 表示直连。
pub fn get_current_proxy_url() -> Option<String> {
    CURRENT_PROXY_URL
        .get()
        .and_then(|lock| lock.read().ok())
        .and_then(|url| url.clone())
}

/// 检查是否正在使用代理
#[allow(dead_code)]
pub fn is_proxy_enabled() -> bool {
    get_current_proxy_url().is_some()
}

/// 构建 HTTP 客户端
fn build_client(proxy_url: Option<&str>) -> Result<Client, String> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .tcp_keepalive(Duration::from_secs(60))
        // 禁用 reqwest 自动解压：防止 reqwest 覆盖客户端原始 accept-encoding header。
        // 响应解压由 response_processor 根据 content-encoding 手动处理。
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd();

    // 有代理地址则使用代理，否则跟随系统代理
    if let Some(url) = proxy_url {
        // 先验证 URL 格式和 scheme
        let parsed = url::Url::parse(url)
            .map_err(|e| format!("Invalid proxy URL '{}': {}", mask_url(url), e))?;

        let scheme = parsed.scheme();
        if !["http", "https", "socks5", "socks5h"].contains(&scheme) {
            return Err(format!(
                "Invalid proxy scheme '{}' in URL '{}'. Supported: http, https, socks5, socks5h",
                scheme,
                mask_url(url)
            ));
        }

        let proxy = reqwest::Proxy::all(url)
            .map_err(|e| format!("Invalid proxy URL '{}': {}", mask_url(url), e))?;
        builder = builder.proxy(proxy);
        log::debug!("[GlobalProxy] Proxy configured: {}", mask_url(url));
    } else {
        // 未设置全局代理时，跟随系统代理。reqwest 内建的自动跟随只在客户端
        // 构建时读取一次系统状态：应用启动后系统代理关闭/切换（代理工具退出、
        // 转为 TUN 模式）会让客户端永远把请求发往失效代理，所有用量查询瞬间
        // 失败且只能靠重启恢复。因此 macOS 上额外做两件事（其余平台行为不变）：
        //   1) 构建时若解析出的系统代理指向本机已无监听的端口，旁路为直连；
        //   2) get() 中节流地检测解析结果变化并按需重建（见 refresh_system_proxy_if_changed）。
        // 代理存活时仍完全交给 reqwest 自动跟随（语义忠实：分协议、NO_PROXY 等）。
        if system_proxy_points_to_loopback() {
            builder = builder.no_proxy();
            log::warn!(
                "[GlobalProxy] System proxy points to localhost, bypassing to avoid recursion"
            );
        } else if cfg!(target_os = "macos") {
            let raw = raw_system_proxy_url();
            if let Some(dead_url) =
                raw.filter(|url| proxy_url_is_loopback(url) && !loopback_proxy_listening(url))
            {
                builder = builder.no_proxy();
                log::warn!(
                    "[GlobalProxy] System proxy {} points to a local port with no listener, bypassing to direct connection",
                    mask_url(&dead_url)
                );
            }
        } else {
            log::debug!("[GlobalProxy] Following system proxy (no explicit proxy configured)");
        }
    }

    builder
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))
}

fn system_proxy_points_to_loopback() -> bool {
    const KEYS: [&str; 6] = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];

    KEYS.iter()
        .filter_map(|key| env::var(key).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .any(|value| proxy_points_to_loopback(&value))
}

fn proxy_points_to_loopback(value: &str) -> bool {
    // 检查是否指向 CC Switch 自己的代理端口
    // 只有指向自己的代理才需要跳过，避免递归
    fn is_cc_switch_proxy_port(port: Option<u16>) -> bool {
        let cc_switch_port = get_proxy_port();
        port == Some(cc_switch_port)
    }

    if let Ok(parsed) = url::Url::parse(value) {
        if let Some(host) = parsed.host_str() {
            // 只有当主机是 loopback 且端口是 CC Switch 的端口时才返回 true
            return host_is_loopback(host) && is_cc_switch_proxy_port(parsed.port());
        }
        return false;
    }

    let with_scheme = format!("http://{value}");
    if let Ok(parsed) = url::Url::parse(&with_scheme) {
        if let Some(host) = parsed.host_str() {
            return host_is_loopback(host) && is_cc_switch_proxy_port(parsed.port());
        }
    }

    false
}

/// 隐藏 URL 中的敏感信息（用于日志）
pub fn mask_url(url: &str) -> String {
    if let Ok(parsed) = url::Url::parse(url) {
        // 隐藏用户名和密码，保留 scheme、host 和端口
        let host = parsed.host_str().unwrap_or("?");
        match parsed.port() {
            Some(port) => format!("{}://{}:{}", parsed.scheme(), host, port),
            None => format!("{}://{}", parsed.scheme(), host),
        }
    } else {
        // URL 解析失败，返回部分内容
        if url.len() > 20 {
            format!("{}...", &url[..20])
        } else {
            url.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn test_mask_url() {
        assert_eq!(mask_url("http://127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(
            mask_url("http://user:pass@127.0.0.1:7890"),
            "http://127.0.0.1:7890"
        );
        assert_eq!(
            mask_url("socks5://admin:secret@proxy.example.com:1080"),
            "socks5://proxy.example.com:1080"
        );
        // 无端口的 URL 不应显示 ":?"
        assert_eq!(
            mask_url("http://proxy.example.com"),
            "http://proxy.example.com"
        );
        assert_eq!(
            mask_url("https://user:pass@proxy.example.com"),
            "https://proxy.example.com"
        );
    }

    #[test]
    fn test_build_client_direct() {
        let result = build_client(None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_client_with_http_proxy() {
        let result = build_client(Some("http://127.0.0.1:7890"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_client_with_socks5_proxy() {
        let result = build_client(Some("socks5://127.0.0.1:1080"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_client_invalid_url() {
        // reqwest::Proxy::all 对某些无效 URL 不会立即报错
        // 使用明确无效的 scheme 来触发错误
        let result = build_client(Some("invalid-scheme://127.0.0.1:7890"));
        assert!(result.is_err(), "Should reject invalid proxy scheme");
    }

    #[test]
    fn test_proxy_points_to_loopback() {
        // 设置 CC Switch 代理端口为 15721（默认值）
        set_proxy_port(15721);

        // 只有指向 CC Switch 自己端口的 loopback 地址才返回 true
        assert!(proxy_points_to_loopback("http://127.0.0.1:15721"));
        assert!(proxy_points_to_loopback("socks5://localhost:15721"));
        assert!(proxy_points_to_loopback("127.0.0.1:15721"));

        // 其他 loopback 端口不应该被跳过（允许使用其他本地代理工具）
        assert!(!proxy_points_to_loopback("http://127.0.0.1:7890"));
        assert!(!proxy_points_to_loopback("socks5://localhost:1080"));

        // 非 loopback 地址不应该被跳过
        assert!(!proxy_points_to_loopback("http://192.168.1.10:7890"));
        assert!(!proxy_points_to_loopback("http://192.168.1.10:15721"));
    }

    #[test]
    fn test_system_proxy_points_to_loopback() {
        let _guard = env_lock().lock().unwrap();

        // 设置 CC Switch 代理端口
        set_proxy_port(15721);

        let keys = [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];

        for key in &keys {
            std::env::remove_var(key);
        }

        // 指向 CC Switch 端口的代理应该被跳过
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:15721");
        assert!(system_proxy_points_to_loopback());

        // 指向其他端口的本地代理不应该被跳过
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:7890");
        assert!(!system_proxy_points_to_loopback());

        // 非 loopback 地址不应该被跳过
        std::env::set_var("HTTP_PROXY", "http://10.0.0.2:7890");
        assert!(!system_proxy_points_to_loopback());

        for key in &keys {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn test_proxy_url_is_loopback() {
        assert!(proxy_url_is_loopback("http://127.0.0.1:7892"));
        assert!(proxy_url_is_loopback("http://localhost:7892"));
        assert!(proxy_url_is_loopback("socks5://127.0.0.1:1080"));
        // [::1] 带 scheme 与裸 host:port 两种写法
        assert!(proxy_url_is_loopback("http://[::1]:7892"));
        assert!(!proxy_url_is_loopback("http://192.168.1.10:7890"));
        assert!(!proxy_url_is_loopback("http://proxy.example.com:8080"));
    }

    #[test]
    fn test_loopback_proxy_listening() {
        // 未监听的 loopback 端口：探测应返回 false
        assert!(!loopback_proxy_listening("http://127.0.0.1:9"));

        // 实际监听中的端口：探测应返回 true
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(loopback_proxy_listening(&format!(
            "http://127.0.0.1:{port}"
        )));

        // 非 loopback 地址不探测，直接视为可用
        assert!(loopback_proxy_listening("http://192.168.1.10:7890"));
    }

    #[test]
    fn test_env_system_proxy_url_priority() {
        let _guard = env_lock().lock().unwrap();

        let keys = [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];
        for key in &keys {
            std::env::remove_var(key);
        }

        // 无环境变量时返回 None
        assert_eq!(env_system_proxy_url(), None);

        // HTTPS_PROXY 优先于 HTTP_PROXY 与 ALL_PROXY
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:8899");
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:7800");
        std::env::set_var("ALL_PROXY", "socks5://127.0.0.1:7801");
        assert_eq!(
            env_system_proxy_url(),
            Some("http://127.0.0.1:8899".to_string())
        );

        // 无 HTTPS 时回退到 HTTP_PROXY
        std::env::remove_var("HTTPS_PROXY");
        assert_eq!(
            env_system_proxy_url(),
            Some("http://127.0.0.1:7800".to_string())
        );

        // 小写变量同样生效
        for key in &keys {
            std::env::remove_var(key);
        }
        std::env::set_var("https_proxy", "http://127.0.0.1:8899");
        assert_eq!(
            env_system_proxy_url(),
            Some("http://127.0.0.1:8899".to_string())
        );

        for key in &keys {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn test_effective_system_proxy_bypasses_dead_loopback() {
        let _guard = env_lock().lock().unwrap();

        let keys = [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];
        for key in &keys {
            std::env::remove_var(key);
        }

        // 环境变量指向无监听的 loopback 端口：应旁路为直连（None）
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:9");
        assert_eq!(current_effective_system_proxy(), None);

        // 环境变量指向存活端口：应保留该代理
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::env::set_var("HTTPS_PROXY", format!("http://127.0.0.1:{port}"));
        assert_eq!(
            current_effective_system_proxy(),
            Some(format!("http://127.0.0.1:{port}"))
        );

        for key in &keys {
            std::env::remove_var(key);
        }
    }

    /// 初始化全局客户端并强制刷新系统代理快照（消除 init 路径差异）
    #[cfg(target_os = "macos")]
    fn setup_global_client_with_current_snapshot() {
        if GLOBAL_CLIENT.get().is_none() {
            let _ = init(None);
        }
        record_baked_system_proxy(None);
    }

    #[cfg(target_os = "macos")]
    fn clear_proxy_env() {
        for key in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            std::env::remove_var(key);
        }
    }

    // 变化检测与重建行为是 macOS 专属（其余平台 refresh 为 no-op）
    #[test]
    #[serial_test::serial]
    #[cfg(target_os = "macos")]
    fn test_no_rebuild_when_system_proxy_unchanged() {
        let _guard = env_lock().lock().unwrap();
        clear_proxy_env();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::env::set_var("HTTPS_PROXY", format!("http://127.0.0.1:{port}"));

        setup_global_client_with_current_snapshot();
        let before = system_proxy_rebuild_count();

        // 代理解析结果未变化：多次触发检测都不应重建客户端
        // （回归防护：若快照未正确写入/读取，这里每次都会误判为变化）
        for _ in 0..3 {
            reset_system_proxy_check_throttle();
            refresh_system_proxy_if_changed();
            assert_eq!(
                system_proxy_rebuild_count(),
                before,
                "unchanged system proxy must not trigger client rebuild"
            );
        }

        clear_proxy_env();
    }

    #[test]
    #[serial_test::serial]
    #[cfg(target_os = "macos")]
    fn test_rebuild_once_when_system_proxy_dies() {
        let _guard = env_lock().lock().unwrap();
        clear_proxy_env();

        // 代理存活时初始化快照，随后关掉监听模拟代理工具退出
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::env::set_var("HTTPS_PROXY", format!("http://127.0.0.1:{port}"));

        setup_global_client_with_current_snapshot();
        let before = system_proxy_rebuild_count();

        drop(listener);
        reset_system_proxy_check_throttle();
        refresh_system_proxy_if_changed();
        let after_death = system_proxy_rebuild_count();
        assert_eq!(
            after_death,
            before + 1,
            "system proxy dying must trigger exactly one rebuild"
        );

        // 代理保持死亡：后续检测不应再次重建
        reset_system_proxy_check_throttle();
        refresh_system_proxy_if_changed();
        assert_eq!(
            system_proxy_rebuild_count(),
            after_death,
            "keep-direct state must not rebuild repeatedly"
        );

        clear_proxy_env();
    }
}
