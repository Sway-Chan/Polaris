//! 代理端口解析（上游 `shared/proxy-ports.ts` 1:1 移植）。
//!
//! localProxyPort（mixedPort 优先，回退 httpPort，再回退默认 7890）。
//! controlApiPort（controlPort 优先，默认 9090）。

#![forbid(unsafe_code)]

/// 默认混合代理端口（对齐业内 7890）。上游 `DEFAULT_MIXED_PORT`。
pub const DEFAULT_MIXED_PORT: u16 = 7890;

/// 默认 clash_api 控制端口（对齐业内 9090）。上游 `DEFAULT_CONTROL_PORT`。
pub const DEFAULT_CONTROL_PORT: u16 = 9090;

/// 端口配置投影（UserConfig 子集）。
pub trait PortConfig {
    fn mixed_port(&self) -> Option<u16>;
    fn http_port(&self) -> Option<u16>;
    fn control_port(&self) -> Option<u16>;
}

/// 本地混合代理端口。mixedPort>0 用之；回退 httpPort>0；再回退默认。上游 `localProxyPort`。
pub fn local_proxy_port<C: PortConfig>(config: &C) -> u16 {
    if let Some(p) = config.mixed_port() {
        if p > 0 {
            return p;
        }
    }
    if let Some(p) = config.http_port() {
        if p > 0 {
            return p;
        }
    }
    DEFAULT_MIXED_PORT
}

/// clash_api 控制端口。controlPort>0 用之；否则默认。上游 `controlApiPort`。
pub fn control_api_port<C: PortConfig>(config: &C) -> u16 {
    match config.control_port() {
        Some(p) if p > 0 => p,
        _ => DEFAULT_CONTROL_PORT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Cfg {
        mixed: Option<u16>,
        http: Option<u16>,
        control: Option<u16>,
    }

    impl PortConfig for Cfg {
        fn mixed_port(&self) -> Option<u16> {
            self.mixed
        }
        fn http_port(&self) -> Option<u16> {
            self.http
        }
        fn control_port(&self) -> Option<u16> {
            self.control
        }
    }

    #[test]
    fn mixed_port_preferred() {
        assert_eq!(
            local_proxy_port(&Cfg {
                mixed: Some(7890),
                http: Some(2080),
                control: None
            }),
            7890
        );
    }

    #[test]
    fn fallback_http_port() {
        assert_eq!(
            local_proxy_port(&Cfg {
                mixed: None,
                http: Some(2080),
                control: None
            }),
            2080
        );
    }

    #[test]
    fn default_when_unset() {
        assert_eq!(
            local_proxy_port(&Cfg {
                mixed: None,
                http: None,
                control: None
            }),
            DEFAULT_MIXED_PORT
        );
    }

    #[test]
    fn zero_treated_as_unset() {
        assert_eq!(
            local_proxy_port(&Cfg {
                mixed: Some(0),
                http: Some(1087),
                control: None
            }),
            1087
        );
    }

    #[test]
    fn control_port() {
        assert_eq!(
            control_api_port(&Cfg {
                mixed: None,
                http: None,
                control: Some(9091)
            }),
            9091
        );
        assert_eq!(
            control_api_port(&Cfg {
                mixed: None,
                http: None,
                control: None
            }),
            DEFAULT_CONTROL_PORT
        );
    }
}
