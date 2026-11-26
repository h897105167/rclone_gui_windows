//! 派生远程类型：插件声明「新建远程」下拉框里多出来的类型。
//!
//! 插件在 `config.json` 顶层写 `remote_types: [{id, name, base_type, defaults}]`，
//! 例如 `{"id":"k-vault","name":"K-Vault","base_type":"webdav"}`。选这个类型建远程时，
//! 写进 rclone 配置的 `type` 永远是 `base_type`（rclone 不认识 `k-vault`），
//! `id` 只用于界面显示和"这个远程归哪个插件管"的判断。
//!
//! 三条必须守住的规则：
//! - 只有**启用**的插件贡献类型。停用/卸载后已建的远程留在 rclone 配置里不动，
//!   用户的数据不跟着插件走。
//! - 界面显示 `name`，逻辑判断一律用 `id`。把显示名当 id 用，改一次措辞就会让
//!   已有远程认不出来。
//! - 派生类型的后端（`base_type`）必须真的存在于当前连接的内核里。官方 rclone
//!   没有 `teldrive` 这类 fork 才有的后端，遇到时要明确报"内核缺后端"，
//!   而不是让插件悄悄再起一个 rclone 进程。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use crate::Ctx;

/// 一个插件声明的派生类型。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derived {
    /// 界面显示用的名字（`name`），可以随语言/措辞变。
    pub name: String,
    /// 真正的类型 id，写进下拉框的 id 列表，也是判断"这条远程归谁"的依据。
    pub id: String,
    /// 落盘时写进 rclone 配置的 `type`。
    pub base_type: String,
    /// 建远程时预填的参数。插件只能预设参数，不能改后端。
    pub defaults: BTreeMap<String, String>,
    /// 声明它的插件 id，用于提示"这个类型来自哪个插件"。
    pub plugin: String,
    /// 远程参数里「这条归我管」的标记键（插件的 `remote_types[].marker`，缺省是 id）。
    ///
    /// 判定一条远程属于谁，看的是**参数里有没有这个键**，不是看它的 `type`
    /// 或显示名：用户可能改过显示名，`type` 又永远是 `base_type`。
    pub marker: String,
    /// 这个类型承接的远程操作（`list` / `upload` / `delete` / `rclone-write`）。
    ///
    /// 空表表示插件只提供类型、不接管任何操作：那种远程仍按 `base_type` 的
    /// 原生方式访问，插件的存在不改变它怎么读写。
    pub ops: Vec<String>,
    /// 早期由主程序代管、现在该交给插件的设置命名空间（插件的 `adopt_legacy`）。
    ///
    /// 主程序只读一次、只读不删：这些命名空间里的东西是用户的旧配置，插件写坏了
    /// 还能重来，删掉就没了。名字全来自插件清单，主程序里不出现任何具体名字。
    pub legacy: Vec<String>,
}

impl Derived {
    /// 建远程时的初始参数：先铺 defaults，再盖上 `type`。
    ///
    /// `type` 最后写，避免插件在 defaults 里放 `type` 覆盖掉 base_type
    /// （插件侧校验已经拒绝这种写法，这里是第二道保险）。
    pub fn initial_params(&self) -> BTreeMap<String, String> {
        let mut p = self.defaults.clone();
        p.insert("type".to_string(), self.base_type.clone());
        p
    }

    /// 插件是否声明会做这个远程操作。
    pub fn takes(&self, op: &str) -> bool {
        self.ops.iter().any(|o| o == op)
    }
}

/// 收集当前**启用**插件声明的派生类型，按 id 排序，id 重复时保留先出现的。
///
/// 扫描是同步读目录：插件目录里通常只有几个包，启动和打开新建对话框时各算一次，
/// 不值得为它引入缓存失效逻辑（插件启停本来就走整表重扫）。
pub fn collect(ctx: &Arc<Ctx>) -> Vec<Derived> {
    let enabled = ctx
        .cfg
        .lock()
        .map(|c| c.plugin_enabled.clone())
        .unwrap_or_default();
    collect_in(
        &crate::plugins::plugins_dir(),
        &enabled,
        &crate::plugins::gui_version(),
    )
}

/// [`collect`] 的可测版本：插件根目录与界面版本都由参数给，不读全局状态。
///
/// 目录读不出来（还没有任何插件）不算错误，返回空表——新建对话框会显示
/// 「没有可选的类型」，而不是编一份类型表。
pub fn collect_in(root: &Path, enabled: &HashMap<String, bool>, gui_version: &str) -> Vec<Derived> {
    let mut manifests: Vec<plugin_host::config::PluginConfig> = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    for entry in rd.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        // 只认新格式插件包（目录 + config.json）；单文件 wasm 没有 config.json，
        // 声明不了 remote_types。
        if plugin_host::config::ConfigFileNames::detect(&dir).is_none() {
            continue;
        }
        // 解析（含 min_gui_version 校验）失败的包在这里静默跳过：插件列表页会
        // 说明它为什么没加载，新建对话框不该因为一个坏包就弹错。没通过校验的插件
        // 本身就不能运行，自然也不该贡献类型。
        let Ok((cfg, _report)) = plugin_host::bundle::load_from_dir(&dir, gui_version) else {
            continue;
        };
        manifests.push(cfg);
    }
    from_manifests(manifests.into_iter(), enabled)
}

/// 筛选并映射成界面用的类型：`enabled` 里没写或写了 `false` 的插件一律不贡献。
fn from_manifests(
    manifests: impl Iterator<Item = plugin_host::config::PluginConfig>,
    enabled: &HashMap<String, bool>,
) -> Vec<Derived> {
    let mut out: Vec<Derived> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for cfg in manifests {
        if !enabled.get(&cfg.id).copied().unwrap_or(false) {
            continue;
        }
        for rt in &cfg.remote_types {
            if seen.iter().any(|s| s == &rt.id) {
                continue;
            }
            seen.push(rt.id.clone());
            out.push(Derived {
                name: rt.name.clone(),
                id: rt.id.clone(),
                base_type: rt.base_type.clone(),
                defaults: rt.defaults.clone(),
                plugin: cfg.id.clone(),
                marker: rt.marker_key(),
                ops: rt.ops.clone(),
                legacy: cfg.adopt_legacy.clone(),
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// 这条类型 id 是不是派生类型；是则返回它的定义。
pub fn find<'a>(list: &'a [Derived], type_id: &str) -> Option<&'a Derived> {
    list.iter().find(|d| d.id == type_id)
}

/// 哪个插件在管这个标记键。
///
/// 标记键是远程参数里的一个普通参数名（插件在自己的 `remote_types[].marker` 里
/// 声明，缺省用它的类型 id）。远程带了这个参数，就说明它是这种类型建出来的，
/// 列目录、上传、删除都该找那个插件做。
///
/// 插件停用或卸载后这里给 `None`。调用方**不能**因此把它当普通后端去访问：
/// 那种远程的读写协议只有插件知道，硬走 `base_type` 的原生方式等于绕过插件的
/// 接口说明，结果往往是半个目录、写坏的文件，或者干脆把令牌发给了错的地址。
pub fn marker_owner<'a>(list: &'a [Derived], marker: &str) -> Option<&'a Derived> {
    list.iter().find(|d| d.marker == marker)
}

/// 从一条远程的参数里看出它归哪个插件管。
///
/// 认的是**参数里有这个标记键**且值不是关闭状态（空串、`0`、`false`、`no`
/// 都算没设）。特意不看 `type`：所有派生类型的 `type` 都是同一个 `base_type`，
/// 看它等于把「普通 webdav」和「插件的 webdav」混成一类。
///
/// 参数直接来自 rclone 的配置（`config dump`/`config get`），因此是字符串表；
/// 别的形状（数字、布尔）按"有值"处理，宁可多认也不要漏认。
pub fn owner_of_params<'a>(
    list: &'a [Derived],
    params: &serde_json::Map<String, serde_json::Value>,
) -> Option<&'a Derived> {
    list.iter()
        .find(|d| marker_is_set(params.get(d.marker.as_str())))
}

/// 标记参数是否处于「开」的状态。
pub fn marker_is_set(v: Option<&serde_json::Value>) -> bool {
    match v {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(serde_json::Value::String(s)) => {
            let s = s.trim();
            !(s.is_empty()
                || s == "0"
                || s.eq_ignore_ascii_case("false")
                || s.eq_ignore_ascii_case("no"))
        }
        // 数组/对象不是参数该有的形状；有内容就算设了，交给插件自己判。
        Some(_) => true,
    }
}

/// 落盘时应该写的 `type`：派生类型换成它的 `base_type`，普通类型原样返回。
pub fn backend_of(list: &[Derived], type_id: &str) -> String {
    match find(list, type_id) {
        Some(d) => d.base_type.clone(),
        None => type_id.to_string(),
    }
}

/// 当前内核支持的后端名集合里是否已有这个后端。
///
/// `providers` 是 `config/providers` 的原始返回（`{"providers": [{"Name": "webdav", ...}]}`）。
/// 返回 `None` 表示拿不到 provider 列表（未连接/没有权限），此时**不要**判定"缺后端"——
/// 无从判断和确认缺失是两件事，把前者说成后者会把用户引去装根本不缺的东西。
pub fn backend_available(providers: &serde_json::Value, base: &str) -> Option<bool> {
    let list = providers.get("providers")?.as_array()?;
    if list.is_empty() {
        return None;
    }
    let want = base.trim().to_ascii_lowercase();
    Some(
        list.iter()
            .filter_map(|p| p.get("Name").and_then(|n| n.as_str()))
            .any(|n| n.eq_ignore_ascii_case(&want)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: &str, name: &str, base: &str, defaults: &[(&str, &str)]) -> Derived {
        Derived {
            id: id.into(),
            name: name.into(),
            base_type: base.into(),
            defaults: defaults
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            plugin: "com.example.plugin".into(),
            // 清单里不写 marker 时用类型 id（`RemoteType::marker_key` 的兜底）
            marker: id.into(),
            ops: Vec::new(),
            legacy: Vec::new(),
        }
    }

    /// 带 marker / ops 的类型，用来测「标记键和类型 id 不一致」与按能力分派。
    fn with_ops(id: &str, marker: &str, ops: &[&str]) -> Derived {
        Derived {
            marker: marker.into(),
            ops: ops.iter().map(|o| o.to_string()).collect(),
            ..d(id, id, "webdav", &[])
        }
    }

    #[test]
    fn backend_of_maps_derived_to_base_type() {
        let list = vec![d("k-vault", "K-Vault", "webdav", &[])];
        assert_eq!(backend_of(&list, "k-vault"), "webdav");
        // 普通类型原样返回，不被派生表影响
        assert_eq!(backend_of(&list, "s3"), "s3");
        assert_eq!(backend_of(&list, "kvault"), "kvault");
    }

    #[test]
    fn find_uses_id_not_name() {
        let list = vec![d("k-vault", "K-Vault", "webdav", &[])];
        assert!(find(&list, "k-vault").is_some());
        // 显示名不能当 id：改措辞不该影响查找
        assert!(find(&list, "K-Vault").is_none());
    }

    #[test]
    fn initial_params_carry_type_last() {
        // 插件即使把 type 写进 defaults，也不能改掉 base_type
        let dt = d(
            "k-vault",
            "K-Vault",
            "webdav",
            &[("type", "s3"), ("vendor", "other")],
        );
        let p = dt.initial_params();
        assert_eq!(p.get("type").map(String::as_str), Some("webdav"));
        assert_eq!(p.get("vendor").map(String::as_str), Some("other"));
    }

    #[test]
    fn backend_available_distinguishes_unknown_from_missing() {
        let providers = serde_json::json!({
            "providers": [{"Name": "s3"}, {"Name": "webdav"}]
        });
        assert_eq!(backend_available(&providers, "webdav"), Some(true));
        assert_eq!(backend_available(&providers, "WebDAV"), Some(true));
        assert_eq!(backend_available(&providers, "teldrive"), Some(false));
        // 没连上 / 没权限：不知道，不等于缺失
        assert_eq!(backend_available(&serde_json::json!({}), "webdav"), None);
        assert_eq!(
            backend_available(&serde_json::json!({"providers": []}), "webdav"),
            None
        );
    }

    /// 造一份最小插件清单：只需要 id 和它声明的远程类型。
    fn manifest(id: &str, types: serde_json::Value) -> plugin_host::config::PluginConfig {
        let v = serde_json::json!({
            "id": id,
            "name": id,
            "version": "1.0.0",
            "entry": "main.wasm",
            "remote_types": types,
        });
        serde_json::from_value(v).expect("最小清单应能反序列化")
    }

    fn enabled_of(pairs: &[(&str, bool)]) -> HashMap<String, bool> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn no_plugin_means_no_derived_types() {
        // 一个插件都没装：新建对话框不该因此凭空多出类型
        let list = from_manifests(std::iter::empty(), &enabled_of(&[]));
        assert!(list.is_empty());
    }

    #[test]
    fn enabled_plugin_contributes_exactly_its_declared_types() {
        let cfg = manifest(
            "kvault",
            serde_json::json!([{"id": "k-vault", "name": "K-Vault", "base_type": "webdav"}]),
        );
        let list = from_manifests(std::iter::once(cfg), &enabled_of(&[("kvault", true)]));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "k-vault");
        assert_eq!(list[0].name, "K-Vault");
        assert_eq!(list[0].base_type, "webdav");
        assert_eq!(list[0].plugin, "kvault");
    }

    #[test]
    fn disabled_or_unlisted_plugin_contributes_nothing() {
        let cfg = manifest(
            "kvault",
            serde_json::json!([{"id": "k-vault", "name": "K-Vault", "base_type": "webdav"}]),
        );
        let off = from_manifests(
            std::iter::once(manifest(
                "kvault",
                serde_json::json!([{"id": "k-vault", "name": "K-Vault", "base_type": "webdav"}]),
            )),
            &enabled_of(&[("kvault", false)]),
        );
        assert!(off.is_empty(), "停用的插件不能贡献类型");
        let unlisted = from_manifests(std::iter::once(cfg), &enabled_of(&[]));
        assert!(unlisted.is_empty(), "没在启用表里出现过，按未启用算");
    }

    #[test]
    fn duplicate_type_id_keeps_the_first_plugin() {
        let a = manifest(
            "plugin-a",
            serde_json::json!([{"id": "k-vault", "name": "A", "base_type": "webdav"}]),
        );
        let b = manifest(
            "plugin-b",
            serde_json::json!([{"id": "k-vault", "name": "B", "base_type": "webdav"}]),
        );
        let list = from_manifests(
            vec![a, b].into_iter(),
            &enabled_of(&[("plugin-a", true), ("plugin-b", true)]),
        );
        assert_eq!(list.len(), 1, "同一个 id 只留一项");
        assert_eq!(list[0].plugin, "plugin-a");
    }

    #[test]
    fn missing_plugin_directory_is_not_an_error() {
        let root = std::env::temp_dir().join("rcgui-no-such-plugins-dir");
        assert!(collect_in(&root, &enabled_of(&[]), "0.3.0").is_empty());
    }

    #[test]
    fn marker_owner_is_none_once_the_declaring_plugin_is_gone() {
        let list = vec![d("kvault", "K-Vault", "webdav", &[("kvault", "1")])];
        assert_eq!(
            marker_owner(&list, "kvault").map(|d| d.plugin.as_str()),
            Some("com.example.plugin")
        );
        // 插件停用/卸载后清单里没有这一项：桥必须停手，别把普通 webdav 当 K-Vault。
        assert!(marker_owner(&[], "kvault").is_none());
        // 显示名不是 id，拿名字来问等于问错人。
        assert!(marker_owner(&list, "K-Vault").is_none());
    }

    #[test]
    fn owner_of_params_reads_the_marker_not_the_backend_type() {
        // 标记键和类型 id 故意取不一样的字符串：判定只能靠参数里的键。
        let list = vec![with_ops(
            "k-vault",
            "kvault",
            &["remote.list", "remote.upload"],
        )];
        let marked = serde_json::json!({"type": "webdav", "kvault": "1", "vendor": "other"});
        let plain = serde_json::json!({"type": "webdav", "vendor": "other"});
        assert_eq!(
            owner_of_params(&list, marked.as_object().unwrap()).map(|d| d.plugin.as_str()),
            Some("com.example.plugin")
        );
        // 普通 webdav 的参数里没有这个键：不能被认领，否则它的读写会被插件接管。
        assert!(owner_of_params(&list, plain.as_object().unwrap()).is_none());
    }

    #[test]
    fn owner_of_params_treats_off_values_as_unset() {
        let list = vec![with_ops("k-vault", "kvault", &["remote.list"])];
        for off in [
            serde_json::json!(""),
            serde_json::json!("0"),
            serde_json::json!("false"),
            serde_json::json!(" NO "),
            serde_json::json!(0),
            serde_json::json!(false),
        ] {
            let p = serde_json::json!({"kvault": off});
            assert!(
                owner_of_params(&list, p.as_object().unwrap()).is_none(),
                "关掉的值不该被当成认领：{}",
                p
            );
        }
        for on in [
            serde_json::json!("1"),
            serde_json::json!("true"),
            serde_json::json!(1),
            serde_json::json!(true),
        ] {
            let p = serde_json::json!({"kvault": on});
            assert!(
                owner_of_params(&list, p.as_object().unwrap()).is_some(),
                "开着就代表认领：{}",
                p
            );
        }
    }

    #[test]
    fn only_declared_ops_are_taken() {
        use crate::remote_ops::{DELETE, LIST, UPLOAD, WRITE};
        let dt = with_ops("k-vault", "kvault", &[LIST, UPLOAD]);
        assert!(dt.takes(LIST));
        assert!(dt.takes(UPLOAD));
        assert!(!dt.takes(DELETE));
        assert!(!dt.takes(WRITE));
        // 什么都没声明：只提供类型，不接管操作。
        let type_only = with_ops("k-vault", "kvault", &[]);
        for op in [LIST, UPLOAD, DELETE, WRITE] {
            assert!(!type_only.takes(op), "{} 不该被接管", op);
        }
    }

    #[test]
    fn manifest_marker_and_ops_reach_the_derived_type() {
        let cfg = manifest(
            "kvault",
            serde_json::json!([{
                "id": "k-vault",
                "name": "K-Vault",
                "base_type": "webdav",
                "marker": "kvault",
                "ops": ["remote.list", "remote.upload", "remote.delete", "remote.rclone-write"],
            }]),
        );
        let list = from_manifests(std::iter::once(cfg), &enabled_of(&[("kvault", true)]));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].marker, "kvault");
        assert_eq!(list[0].ops.len(), 4);
        assert!(list[0].takes("remote.delete"));
        // 清单不写 marker 时用类型 id 兜底，老插件不用改配置。
        let old = manifest(
            "kvault",
            serde_json::json!([{"id": "k-vault", "name": "K-Vault", "base_type": "webdav"}]),
        );
        let old = from_manifests(std::iter::once(old), &enabled_of(&[("kvault", true)]));
        assert_eq!(old[0].marker, "k-vault");
        assert!(old[0].ops.is_empty());
    }
}
