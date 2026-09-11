//! 只读 CCSwitch 当前 Codex 供应商，复用其 JavaScript 额度查询配置。

use crate::consumption::ConsumptionChart;
use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use rquickjs::{Context as JsContext, Runtime};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use std::fs;
use std::time::{Duration, Instant};

/// 中转站余额；以百万分之一单位保存，None 表示没有可用额度。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayBalance {
    pub chart: ConsumptionChart,
    pub provider_id: String,
    pub name: String,
    pub remaining: Option<i64>,
    pub unit: String,
    pub detail: String,
}

/// 去掉小数，显示 0–999；保留原始余额供阈值与消耗统计使用。
pub fn format_balance(amount: i64) -> String {
    (amount / 1_000_000).clamp(0, 999).to_string()
}

impl RelayBalance {
    /// 无余额显示 --；整数余额最大显示 999，不改变采样精度。
    pub fn display_text(&self) -> String {
        self.remaining
            .map(format_balance)
            .unwrap_or_else(|| "--".to_owned())
    }
}

/// 查询选中供应商；官方或未安装 CCSwitch 返回 None，中转站不可查询时返回 -- 状态。
pub fn query_current_balance() -> Result<Option<RelayBalance>> {
    let directory = dirs::home_dir()
        .context("无法定位用户目录")?
        .join(".cc-switch");
    let path = directory.join("cc-switch.db");
    if !path.exists() {
        return Ok(None);
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("无法只读打开 CCSwitch 数据库")?;
    db.busy_timeout(Duration::from_secs(2))?;
    let settings_path = directory.join("settings.json");
    let selected = if settings_path.exists() {
        let settings: Value = serde_json::from_str(&fs::read_to_string(settings_path)?)?;
        settings["currentProviderCodex"].as_str().map(str::to_owned)
    } else {
        None
    };
    query_selected_balance(&db, selected)
}

/// 本机 settings 的当前选择优先于数据库标记；官方不执行中转站脚本。
fn query_selected_balance(
    db: &Connection,
    selected: Option<String>,
) -> Result<Option<RelayBalance>> {
    let selected = match selected {
        Some(id) => Some(id),
        None => db
            .query_row(
                "SELECT id FROM providers WHERE app_type='codex' AND is_current=1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?,
    };
    let Some(id) = selected else { return Ok(None) };
    let (name, category, config, meta): (String, Option<String>, String, String) = db.query_row(
        "SELECT name, category, settings_config, meta FROM providers WHERE app_type='codex' AND id=?1",
        [&id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).context("CCSwitch 当前供应商不存在")?;
    if category.as_deref() == Some("official") {
        return Ok(None);
    }
    let mut balance = RelayBalance {
        chart: ConsumptionChart::default(),
        provider_id: id,
        name,
        remaining: None,
        unit: String::new(),
        detail: "未配置或未启用额度查询".to_owned(),
    };
    let meta: Value = serde_json::from_str(&meta).context("CCSwitch 额度配置格式错误")?;
    let script = &meta["usage_script"];
    if script["enabled"] != true {
        return Ok(Some(balance));
    }
    let config: Value = serde_json::from_str(&config).context("CCSwitch 供应商配置格式错误")?;
    let proxy: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key='global_proxy_url'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match execute_script(script, &config, proxy.as_deref()).and_then(|data| parse_balance(&data)) {
        Ok((remaining, unit)) => {
            balance.remaining = remaining;
            balance.unit = unit;
            balance.detail = if remaining.is_some() {
                "已读取 CCSwitch 余额"
            } else {
                "查询结果没有可用余额"
            }
            .to_owned();
        }
        Err(error) => balance.detail = format!("余额查询失败：{error}"),
    }
    Ok(Some(balance))
}

/// 从对象或单一余额套餐中取 remaining；多余额套餐不猜测求和，保持不可用状态。
fn parse_balance(data: &Value) -> Result<(Option<i64>, String)> {
    let data = if let Some(items) = data.as_array() {
        let mut balances = items
            .iter()
            .filter(|item| item["remaining"].is_number() && item["isValid"] != false);
        let first = balances.next();
        if balances.next().is_some() {
            bail!("存在多个余额套餐，无法确定应显示哪一个")
        }
        match first {
            Some(item) => item,
            None => return Ok((None, String::new())),
        }
    } else {
        data
    };
    if data["isValid"] == false {
        bail!("CCSwitch 额度查询返回无效状态")
    }
    let unit = data["unit"].as_str().unwrap_or("").to_owned();
    let Some(value) = data.get("remaining").filter(|value| !value.is_null()) else {
        return Ok((None, unit));
    };
    let value = value.as_f64().context("余额不是数值")?;
    if !value.is_finite() || value.abs() >= (i64::MAX / 1_000_000) as f64 {
        bail!("余额超出数值范围")
    }
    let amount = (value * 1_000_000.0).round() as i64;
    Ok((Some(amount), unit))
}

/// 解析供应商 TOML 的实际 model_provider 对应 base_url，供脚本占位符使用。
fn provider_base_url(config: &Value) -> Result<String> {
    let config: toml::Value =
        toml::from_str(config["config"].as_str().context("缺少 Codex config")?)
            .context("CCSwitch Codex TOML 格式错误")?;
    let provider = config
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .context("缺少 model_provider")?;
    Ok(config
        .get("model_providers")
        .and_then(|v| v.get(provider))
        .and_then(|v| v.get("base_url"))
        .and_then(toml::Value::as_str)
        .context("缺少供应商 base_url")?
        .trim_end_matches('/')
        .to_owned())
}

/// 按 CCSwitch 约定替换变量，执行 request 与 extractor；密钥只在进程内存中使用。
fn execute_script(script: &Value, config: &Value, proxy: Option<&str>) -> Result<Value> {
    if script["language"].as_str() != Some("javascript") {
        bail!("仅支持 CCSwitch JavaScript 额度脚本")
    }
    let mut code = script["code"].as_str().context("缺少额度脚本")?.to_owned();
    let base_url = match script["baseUrl"].as_str().filter(|value| !value.is_empty()) {
        Some(value) => value.trim_end_matches('/').to_owned(),
        None => provider_base_url(config)?,
    };
    for (name, value) in [
        ("baseUrl", base_url.as_str()),
        (
            "apiKey",
            script["apiKey"]
                .as_str()
                .filter(|v| !v.is_empty())
                .or_else(|| config["auth"]["OPENAI_API_KEY"].as_str())
                .unwrap_or(""),
        ),
        ("accessToken", script["accessToken"].as_str().unwrap_or("")),
        ("userId", script["userId"].as_str().unwrap_or("")),
    ] {
        code = code.replace(&format!("{{{{{name}}}}}"), value);
    }
    let request = evaluate(&code, None)?;
    let url = request["url"].as_str().context("额度脚本缺少请求 URL")?;
    let method = request["method"]
        .as_str()
        .context("额度脚本缺少请求方法")?
        .parse::<reqwest::Method>()
        .context("额度请求方法无效")?;
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(
            script["timeout"].as_u64().unwrap_or(10).clamp(2, 30),
        ))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(proxy) = proxy.filter(|value| !value.is_empty()) {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy).map_err(|_| anyhow::anyhow!("CCSwitch 代理地址无效"))?,
        );
    }
    let client = builder.build().context("无法创建额度请求客户端")?;
    let mut http = client.request(method, url);
    if let Some(headers) = request["headers"].as_object() {
        for (key, value) in headers {
            http = http.header(key, value.as_str().context("额度请求头不是字符串")?);
        }
    }
    if let Some(body) = request["body"].as_str() {
        http = http.body(body.to_owned());
    }
    // 不把服务端正文、带凭据的 URL 或 JS 异常写到日志。
    let response = http
        .send()
        .map_err(|_| anyhow::anyhow!("额度网络请求失败或超时"))?;
    if !response.status().is_success() {
        bail!("额度接口 HTTP {}", response.status().as_u16())
    }
    let response: Value = serde_json::from_str(&response.text().context("无法读取额度响应")?)
        .context("额度接口未返回 JSON")?;
    evaluate(&code, Some(&response))
}

/// 使用无文件/网络能力的 QuickJS，限制脚本 CPU、内存和栈，执行一次配置或提取函数。
fn evaluate(code: &str, response: Option<&Value>) -> Result<Value> {
    let runtime = Runtime::new().map_err(|_| anyhow::anyhow!("无法创建额度脚本运行时"))?;
    runtime.set_memory_limit(16 * 1024 * 1024);
    runtime.set_max_stack_size(256 * 1024);
    let deadline = Instant::now() + Duration::from_secs(2);
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() > deadline)));
    let context =
        JsContext::full(&runtime).map_err(|_| anyhow::anyhow!("无法创建额度脚本上下文"))?;
    context.with(|ctx| {
        let code = code.trim().trim_end_matches(';');
        let expression = match response {
            None => format!("JSON.stringify(({code}).request)"),
            Some(response) => format!("JSON.stringify(({code}).extractor({response}))"),
        };
        let json: String = ctx
            .eval(expression)
            .map_err(|_| anyhow::anyhow!("额度脚本执行失败或超时"))?;
        serde_json::from_str(&json).context("额度脚本返回格式错误")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模拟 CCSwitch 官方/中转站选择和旧当前标记，验证无余额不会冒用官方额度。
    #[test]
    fn follows_selected_provider_instead_of_old_current_flag() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE providers (id TEXT, app_type TEXT, name TEXT, category TEXT, settings_config TEXT, meta TEXT, is_current INTEGER);
            INSERT INTO providers VALUES ('official', 'codex', 'OpenAI', 'official', '{}', '{}', 1);
            INSERT INTO providers VALUES ('relay', 'codex', 'Relay', NULL, '{}', '{}', 0);").unwrap();
        let relay = query_selected_balance(&db, Some("relay".to_owned()))
            .unwrap()
            .unwrap();
        assert_eq!(relay.provider_id, "relay");
        assert_eq!(relay.remaining, None);
        assert_eq!(relay.display_text(), "--");
        assert!(
            query_selected_balance(&db, Some("official".to_owned()))
                .unwrap()
                .is_none()
        );
        assert!(query_selected_balance(&db, None).unwrap().is_none());
        assert!(query_selected_balance(&db, Some("deleted".to_owned())).is_err());
    }

    /// 验证余额去小数且封顶 999，同时仍保留高余额的原始采样值。
    #[test]
    fn formats_three_integer_digits() {
        for (amount, text) in [
            (148_900_000, "148"),
            (22_300_000, "22"),
            (1_481_000_000, "999"),
            (999_960_000, "999"),
            (0, "0"),
            (-1_200_000, "0"),
        ] {
            assert_eq!(format_balance(amount), text);
        }
        assert_eq!(
            parse_balance(&serde_json::json!({"remaining":1481}))
                .unwrap()
                .0,
            Some(1_481_000_000)
        );
    }

    /// 验证真实 CCSwitch 形状的脚本提取及无效、多套餐、无余额响应。
    #[test]
    fn extracts_balance_without_guessing() {
        let script = "({request:{url:'https://example.com',method:'GET'},extractor:r=>({remaining:r.quota/500000,unit:'USD'})})";
        assert_eq!(evaluate(script, None).unwrap()["method"], "GET");
        let result = evaluate(script, Some(&serde_json::json!({"quota":74050000}))).unwrap();
        assert_eq!(
            parse_balance(&result).unwrap(),
            (Some(148_100_000), "USD".to_owned())
        );
        assert_eq!(
            parse_balance(&serde_json::json!({"remaining":null}))
                .unwrap()
                .0,
            None
        );
        assert!(parse_balance(&serde_json::json!({"isValid":false})).is_err());
        assert!(parse_balance(&serde_json::json!([{"remaining":1},{"remaining":2}])).is_err());
    }

    /// 只读真机查询，不写键盘、注册表或 CCSwitch 数据库。
    #[test]
    #[ignore = "需要本机 CCSwitch 配置和可访问的额度接口"]
    fn queries_real_ccswitch_balance() {
        let balance = query_current_balance().unwrap().expect("当前未选择中转站");
        assert!(balance.remaining.is_some(), "{}", balance.detail);
        println!(
            "{}：{} {}",
            balance.name,
            balance.display_text(),
            balance.unit
        );
    }
}
