//! 数据格式迁移。

use super::model::{FORMAT_VERSION, Store};
use anyhow::{Result, bail};

/// 把旧格式数据升级到当前 `FORMAT_VERSION`，按版本号逐级升级。
pub fn migrate(mut store: Store) -> Result<Store> {
    if store.format_version > FORMAT_VERSION {
        bail!(
            "数据文件版本 {} 高于本程序支持的 {}，请升级程序",
            store.format_version,
            FORMAT_VERSION
        );
    }

    // v1 → v2：应用模块整体移除。`Store.apps` / `Profile.apps` / `Profile.app_overrides`
    // 已从模型中删除，旧文件里的这些键在反序列化阶段就被忽略，这里只抬版本号；
    // 文件里仍残留应用定义时，`StoreHandle::new` 会打出显式告警。
    if store.format_version < 2 {
        store.format_version = 2;
    }

    // v2 → v3：新增 `settings.geoip`。反序列化时缺失字段由 `#[serde(default)]`
    // 补成默认值（默认开启自动更新、间隔 7 天、使用内置 DB-IP 源），这里只抬版本号。
    if store.format_version < 3 {
        store.format_version = 3;
    }

    // v3 → v4：新增 `settings.base_url`（订阅链接前缀）。旧文件默认为空。
    if store.format_version < 4 {
        store.format_version = 4;
    }

    // v4 → v5：新增 `manual_nodes`（手动分享链接节点）。旧文件默认为空。
    if store.format_version < 5 {
        store.format_version = 5;
    }

    debug_assert_eq!(store.format_version, FORMAT_VERSION);
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_file_with_apps_upgrades_to_v2() {
        // 旧文件里的 apps 键在 v2 中已不存在，反序列化时被忽略；升级必须不报错，
        // 且不动其他字段。
        let text = concat!(
            "format_version: 1\n",
            "revision: 3\n",
            "apps:\n",
            "- id: openai\n",
            "  name: OpenAI\n",
            "  target_group: 美国节点\n",
            "  rules: []\n",
        );
        let store: Store = serde_yaml_ng::from_str(text).unwrap();
        let store = migrate(store).unwrap();
        assert_eq!(store.format_version, FORMAT_VERSION);
        assert_eq!(store.revision, 3, "迁移不得改动其他字段");
    }

    #[test]
    fn future_version_is_rejected() {
        let text = format!("format_version: {}\n", FORMAT_VERSION + 1);
        let store: Store = serde_yaml_ng::from_str(&text).unwrap();
        assert!(migrate(store).is_err());
    }

    #[test]
    fn v2_file_gets_default_geoip_settings() {
        let text = concat!(
            "format_version: 2\n",
            "revision: 7\n",
            "settings:\n",
            "  region_source: ip_first\n",
        );
        let store: Store = serde_yaml_ng::from_str(text).unwrap();
        let store = migrate(store).unwrap();
        assert_eq!(store.format_version, FORMAT_VERSION);
        assert_eq!(
            store.settings.region_source,
            crate::store::model::RegionSource::IpFirst
        );
        // v3 新增字段必须是可用的默认值，而不是「关闭/0 天」。
        assert!(store.settings.geoip.auto_update);
        assert_eq!(store.settings.geoip.interval_days, 7);
        assert!(store.settings.geoip.url.is_empty());
        // v4 新增 BASEURL，默认为空（展示相对路径）。
        assert!(store.settings.base_url.is_empty());
        // v5 新增手动节点，默认为空。
        assert!(store.manual_nodes.is_empty());
    }
}
