//! Reading usage: Claude Code's login (OAuth) or a claude.ai session, plus the update check.
//!
//! Secrets rule: tokens, cookies and Authorization headers never go into an error message,
//! the log or anything shown in the UI.

use crate::store::{self, log, now, truthy, Settings};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// Claude Code's public OAuth client id: built into Claude Code and the same for every user.
// It is not a secret; it only identifies the app when renewing an expired login.
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_TOKEN_URLS: [&str; 2] = [
    "https://platform.claude.com/v1/oauth/token",
    "https://console.anthropic.com/v1/oauth/token",
];
const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
// Update check: public GitHub API, no login or token is sent.
pub const REPO: &str = "Synt3x/ClaudeUsage";
pub const RELEASES_URL: &str = "https://github.com/Synt3x/ClaudeUsage/releases/latest";
pub const USAGE_URL: &str = "https://claude.ai/settings/usage";
const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36";

const WINDOWS: [(&str, &str, bool); 4] = [
    ("five_hour", "5-hour session", false),
    ("seven_day", "Weekly, all models", true),
    ("seven_day_opus", "Weekly, Opus", true),
    ("seven_day_sonnet", "Weekly, Sonnet", true),
];

// Fields of the usage response the app knows about. Anything else that turns up non-empty
// (for example a reset you can use) is written to app.log so it can be added later.
const KNOWN_FIELDS: [&str; 11] = [
    "five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet", "limits", "spend",
    "extra_usage", "promo_credits", "weekly_scoped_shares", "seven_day_breakdown",
    "member_dashboard_available",
];
const INFO_FIELDS: [&str; 2] = ["pace", "indicator"]; // informational, not something you can use
// Limit-reset programs (asked for with ?at_wall=1&cedar_ember=1); parsed on their own below.
pub const RESET_FIELDS: [&str; 2] = ["juniper_tide", "cedar_ember"];

// ----------------------------------------------------------------- types

#[derive(Clone, Debug, PartialEq)]
pub struct Money {
    pub amount: f64,
    pub currency: String,
}

impl Money {
    fn to_json(&self) -> Value {
        json!([self.amount, self.currency])
    }
    fn from_json(v: &Value) -> Option<Money> {
        let a = v.as_array()?;
        Some(Money { amount: a.first()?.as_f64()?, currency: a.get(1)?.as_str()?.to_string() })
    }
}

#[derive(Clone, Debug)]
pub struct Limit {
    pub key: String,
    pub pct: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub label: String,
    pub weekly: bool,
    pub note: String,
}

/// Something on the account that can be used: a limit reset, an offer, promo credits...
#[derive(Clone, Debug)]
pub struct Extra {
    pub key: String,
    pub name: String,
    pub amount: Option<Money>,
    pub count: Option<Value>,
    pub expires: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default)]
pub struct Credits {
    pub extras: Vec<Extra>,
    pub enabled: bool,
    pub used: Option<Money>,
    pub limit: Option<Money>,
    pub balance: Option<Money>,
    pub balance_expires: Option<DateTime<Utc>>,
    pub promo: Option<Money>,
    pub promo_expires: Option<DateTime<Utc>>,
    pub limit_reached: bool,
}

#[derive(Clone, Debug)]
pub struct Reading {
    pub usage: Vec<Limit>,
    pub plan: String,
    pub credits: Option<Credits>,
    pub source: String,
}

#[derive(Debug)]
pub enum FetchError {
    /// The usage endpoint answered 429: wait this many seconds before asking again.
    Rate(u64),
    Msg(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Rate(_) => write!(f, "rate limited"),
            FetchError::Msg(m) => write!(f, "{m}"),
        }
    }
}

fn err<T>(m: impl Into<String>) -> Result<T, FetchError> {
    Err(FetchError::Msg(m.into()))
}

// ----------------------------------------------------------------- small JSON helpers

static NULL: Value = Value::Null;

fn g<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&NULL)
}

/// Python's `a or b or c`: the first truthy value.
fn first_truthy<'a>(vals: &[&'a Value]) -> &'a Value {
    vals.iter().copied().find(|v| truthy(v)).unwrap_or(&NULL)
}

fn is_int(v: &Value) -> bool {
    v.is_i64() || v.is_u64()
}

/// How Python prints a JSON value with str().
pub fn value_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => {
            if is_int(v) {
                n.to_string()
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                if f.fract() == 0.0 && f.abs() < 1e16 {
                    format!("{f:.1}")
                } else {
                    f.to_string()
                }
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
        None => String::new(),
    }
}

pub fn parse_time(v: &Value) -> Option<DateTime<Utc>> {
    parse_time_str(v.as_str()?)
}

pub fn parse_time_str(s: &str) -> Option<DateTime<Utc>> {
    if s.is_empty() {
        return None;
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok().map(|n| n.and_utc())
}

fn iso(t: &Option<DateTime<Utc>>) -> Value {
    match t {
        Some(t) => Value::String(t.to_rfc3339()),
        None => Value::Null,
    }
}

// ----------------------------------------------------------------- parsing

/// Return the limits in display order.
///
/// Prefers the generic "limits" list, which also covers per-model weekly limits
/// such as Fable (and any added later). Falls back to the fixed fields.
pub fn parse_usage(j: &Value) -> Result<Vec<Limit>, FetchError> {
    let mut out: Vec<Limit> = Vec::new();
    fn put(out: &mut Vec<Limit>, l: Limit) {
        match out.iter_mut().find(|x| x.key == l.key) {
            Some(x) => *x = l,
            None => out.push(l),
        }
    }
    let mut shares: Vec<(i64, &Value)> = Vec::new();
    if let Some(a) = g(j, "weekly_scoped_shares").as_array() {
        for s in a {
            shares.push((g(s, "limit_index").as_i64().unwrap_or(-1), s));
        }
    }
    if let Some(lims) = g(j, "limits").as_array() {
        for (idx, lim) in lims.iter().enumerate() {
            let kind = g(lim, "kind");
            let pct = match g(lim, "percent") {
                Value::Number(n) => n.as_f64().unwrap_or(0.0),
                Value::String(s) => match s.trim().parse::<f64>() {
                    Ok(f) => f,
                    Err(_) => continue,
                },
                _ => continue,
            };
            let resets_at = parse_time(g(lim, "resets_at"));
            let mut l = Limit { key: String::new(), pct, resets_at, label: String::new(), weekly: false, note: String::new() };
            match kind.as_str() {
                Some("session") => {
                    l.key = "five_hour".into();
                    l.label = "5-hour session".into();
                }
                Some("weekly_all") => {
                    l.key = "seven_day".into();
                    l.label = "Weekly, all models".into();
                    l.weekly = true;
                }
                _ => {
                    let scope = g(lim, "scope");
                    let name = value_str(first_truthy(&[
                        g(g(scope, "model"), "display_name"),
                        g(g(scope, "surface"), "display_name"),
                        kind,
                    ]));
                    l.key = "weekly_".to_string()
                        + &name.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>();
                    l.label = format!("Weekly, {name}");
                    l.weekly = g(lim, "group").as_str() == Some("weekly");
                    if let Some((_, share)) = shares.iter().find(|(i, _)| *i == idx as i64) {
                        let a = g(share, "allowance_percent_of_weekly");
                        if truthy(a) {
                            if let Some(f) = a.as_f64() {
                                l.note = format!("Can use up to {f:.0}% of your weekly limit");
                            }
                        }
                    }
                }
            }
            put(&mut out, l);
        }
    }
    if out.is_empty() {
        // older response format
        for (key, label, weekly) in WINDOWS {
            let w = g(j, key);
            if let Some(u) = g(w, "utilization").as_f64() {
                put(&mut out, Limit {
                    key: key.into(),
                    pct: u,
                    resets_at: parse_time(g(w, "resets_at")),
                    label: label.into(),
                    weekly,
                    note: String::new(),
                });
            }
        }
    }
    if !out.iter().any(|l| l.key == "five_hour") {
        return err("No 5-hour data in the response");
    }
    Ok(out)
}

/// {"amount_minor": 1234, "exponent": 2, "currency": "USD"} (or a bare minor-unit number) -> 12.34 USD.
fn money(m: &Value, currency: Option<&str>, decimals: i32) -> Option<Money> {
    let cur = |c: Option<&str>| c.filter(|s| !s.is_empty()).unwrap_or("USD").to_string();
    if let Some(o) = m.as_object() {
        if let Some(a) = o.get("amount_minor").filter(|v| !v.is_null()) {
            let a = a.as_f64()?;
            let e = o.get("exponent").filter(|v| truthy(v)).and_then(|v| v.as_i64()).unwrap_or(0);
            let c = o.get("currency").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).or(currency);
            return Some(Money { amount: a / 10f64.powi(e as i32), currency: cur(c) });
        }
    }
    if let Value::Number(n) = m {
        return Some(Money { amount: n.as_f64()? / 10f64.powi(decimals), currency: cur(currency) });
    }
    None
}

/// First expires_at / expiry style timestamp anywhere inside obj.
fn find_expiry(obj: &Value) -> Option<DateTime<Utc>> {
    match obj {
        Value::Object(o) => {
            for (k, v) in o {
                if k.to_lowercase().contains("expir") {
                    if let Some(t) = v.as_str().and_then(parse_time_str) {
                        return Some(t);
                    }
                }
            }
            o.values().find_map(find_expiry)
        }
        Value::Array(a) => a.iter().find_map(find_expiry),
        _ => None,
    }
}

/// Usage credits (extra usage) and promo credits, or None when the response has none.
pub fn parse_credits(j: &Value) -> Option<Credits> {
    let sp = g(j, "spend");
    let ex = g(j, "extra_usage");
    if !truthy(sp) && !truthy(ex) {
        return None;
    }
    let cur = g(ex, "currency").as_str();
    let dec = g(ex, "decimal_places").as_i64().map(|d| d as i32).unwrap_or(2);
    let used = money(g(sp, "used"), None, 2).or_else(|| money(g(ex, "used_credits"), cur, dec));
    let limit = money(g(sp, "limit"), None, 2)
        .or_else(|| money(g(sp, "cap"), None, 2))
        .or_else(|| money(g(ex, "monthly_limit"), cur, dec));
    let balance = money(g(sp, "balance"), None, 2);
    let promo = g(j, "promo_credits");
    let promo_amt = money(promo, None, 2).or_else(|| {
        if promo.is_object() {
            money(first_truthy(&[g(promo, "balance"), g(promo, "remaining"), g(promo, "amount")]), None, 2)
        } else {
            None
        }
    });
    Some(Credits {
        extras: parse_extras(j),
        enabled: truthy(g(sp, "enabled")) || truthy(g(ex, "is_enabled")),
        used,
        limit,
        balance,
        balance_expires: find_expiry(g(sp, "balance")),
        promo: promo_amt,
        promo_expires: find_expiry(promo),
        limit_reached: truthy(g(ex, "spend_limit_reached")),
    })
}

/// Limit resets you can use right now, from the two reset programs in the usage response.
fn parse_resets(j: &Value) -> Vec<Extra> {
    let mut out = Vec::new();
    let jt = g(j, "juniper_tide");
    if jt.is_object() && truthy(g(jt, "available")) {
        out.push(Extra {
            key: "juniper_tide".into(),
            name: "Weekly limit reset".into(),
            amount: None,
            count: None,
            expires: parse_time(g(jt, "weekly_resets_at")).or_else(|| find_expiry(jt)),
        });
    }
    // cedar_ember: reset grants, e.g. {"id": "...", "type": "full", "resets_left": 1,
    // "ends_at": "<timestamp>", "usable_now": true, "label": "One full limit reset"}.
    if let Some(grants) = g(g(j, "cedar_ember"), "grants").as_array() {
        for gr in grants {
            let left = g(gr, "resets_left");
            if !gr.is_object() || !truthy(left) || truthy(g(gr, "paused")) {
                continue; // used up or paused
            }
            let mut name = if g(gr, "type").as_str() == Some("full") {
                "Full reset".to_string()
            } else {
                let l = g(gr, "label");
                if truthy(l) { value_str(l) } else { "Limit reset".into() }
            };
            if !truthy(g(gr, "usable_now")) {
                name += " (not usable yet)";
            }
            let id = match g(gr, "id") {
                Value::Null => String::new(),
                v => value_str(v),
            };
            out.push(Extra {
                key: format!("cedar_ember:{id}"),
                name,
                amount: None,
                count: Some(left.clone()),
                expires: parse_time(g(gr, "ends_at")).or_else(|| find_expiry(gr)),
            });
        }
    }
    out
}

/// Anything else on the account that could be used: resets, offers, promos, credits.
///
/// The usage response has many unnamed fields that are normally empty. When one of them is
/// filled (and isn't just another usage window), it's shown as an extra with whatever amount,
/// count and expiry it carries.
fn parse_extras(j: &Value) -> Vec<Extra> {
    let mut out = parse_resets(j);
    let mut items: Vec<(&str, &Value)> = Vec::new();
    if let Some(o) = j.as_object() {
        for (k, v) in o {
            let k = k.as_str();
            if !KNOWN_FIELDS.contains(&k) && !INFO_FIELDS.contains(&k) && !RESET_FIELDS.contains(&k) {
                items.push((k, v));
            }
        }
    }
    let promo = g(j, "promo_credits");
    if !promo.is_null() && promo != &json!([]) && promo != &json!({}) && promo != &json!(false) {
        items.push(("promo_credits", promo));
    }
    for (k, v) in items {
        if !truthy(v) {
            continue;
        }
        if let Some(o) = v.as_object() {
            if o.contains_key("utilization") {
                continue; // a usage window; the limits list covers it
            }
            if g(v, "is_enabled") == &json!(false) && g(v, "enabled") != &json!(true) {
                continue;
            }
            if g(v, "eligible") == &json!(false) || g(v, "available") == &json!(false) {
                continue; // a program you're not in, or nothing to use right now
            }
        }
        let mut name = String::new();
        if v.is_object() {
            let n = first_truthy(&[g(v, "display_name"), g(v, "title"), g(v, "name"), g(v, "label")]);
            if let Some(s) = n.as_str() {
                name = s.to_string();
            }
        }
        if name.is_empty() {
            name = capitalize(&k.replace('_', " "));
        }
        let mut amount = if v.is_object() { money(v, None, 2) } else { None };
        if v.is_object() && amount.is_none() {
            amount = money(first_truthy(&[g(v, "balance"), g(v, "remaining"), g(v, "amount")]), None, 2);
        }
        let mut count = None;
        match v {
            Value::Object(_) => {
                for ck in ["count", "remaining_count", "available", "quantity", "resets_remaining", "remaining"] {
                    let c = g(v, ck);
                    if is_int(c) {
                        count = Some(c.clone());
                        break;
                    }
                }
            }
            Value::Number(_) if is_int(v) => count = Some(v.clone()),
            Value::Array(a) => count = Some(json!(a.len())),
            _ => {}
        }
        out.push(Extra { key: k.to_string(), name, amount, count, expires: find_expiry(v) });
    }
    out
}

fn log_new_fields(j: &Value) {
    static LOGGED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let Some(o) = j.as_object() else { return };
    let mut guard = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    let logged = guard.get_or_insert_with(HashSet::new);
    for (k, v) in o {
        let empty = v.is_null() || v == &json!([]) || v == &json!({}) || v == &json!(false);
        if !KNOWN_FIELDS.contains(&k.as_str()) && !empty && !logged.contains(k) {
            logged.insert(k.clone());
            let s: String = v.to_string().chars().take(1500).collect();
            log(&format!("New usage field '{k}': {s}"));
        }
    }
}

// ----------------------------------------------------------------- saved last reading

pub fn extra_to_json(x: &Extra) -> Value {
    json!({"key": x.key, "name": x.name, "amount": x.amount.as_ref().map(|m| m.to_json()),
           "count": x.count, "expires": iso(&x.expires)})
}

pub fn extra_from_json(v: &Value) -> Option<Extra> {
    Some(Extra {
        key: g(v, "key").as_str()?.to_string(),
        name: g(v, "name").as_str().unwrap_or("").to_string(),
        amount: Money::from_json(g(v, "amount")),
        count: Some(g(v, "count").clone()).filter(|c| !c.is_null()),
        expires: parse_time(g(v, "expires")),
    })
}

fn credits_to_json(c: &Credits) -> Value {
    let m = |x: &Option<Money>| x.as_ref().map(|m| m.to_json()).unwrap_or(Value::Null);
    json!({
        "extras": c.extras.iter().map(extra_to_json).collect::<Vec<_>>(),
        "enabled": c.enabled, "used": m(&c.used), "limit": m(&c.limit), "balance": m(&c.balance),
        "balance_expires": iso(&c.balance_expires), "promo": m(&c.promo),
        "promo_expires": iso(&c.promo_expires), "limit_reached": c.limit_reached,
    })
}

fn credits_from_json(v: &Value) -> Credits {
    Credits {
        extras: g(v, "extras").as_array().map(|a| a.iter().filter_map(extra_from_json).collect()).unwrap_or_default(),
        enabled: truthy(g(v, "enabled")),
        used: Money::from_json(g(v, "used")),
        limit: Money::from_json(g(v, "limit")),
        balance: Money::from_json(g(v, "balance")),
        balance_expires: parse_time(g(v, "balance_expires")),
        promo: Money::from_json(g(v, "promo")),
        promo_expires: parse_time(g(v, "promo_expires")),
        limit_reached: truthy(g(v, "limit_reached")),
    }
}

pub fn save_last_usage(r: &Reading) {
    let mut usage = Map::new();
    for l in &r.usage {
        usage.insert(l.key.clone(), json!({"pct": l.pct, "resets_at": iso(&l.resets_at), "label": l.label,
                                           "weekly": l.weekly, "note": l.note}));
    }
    let data = json!({"t": now(), "plan": r.plan, "source": r.source,
                      "credits": r.credits.as_ref().map(credits_to_json), "usage": usage});
    store::save_json(&store::path("last_usage.json"), &data);
}

/// Last good reading, so a restart shows numbers right away instead of asking the server again.
pub fn load_last_usage() -> Option<(Reading, f64)> {
    let d = store::load_json(&store::path("last_usage.json"))?;
    let u = g(&d, "usage").as_object()?;
    if !u.contains_key("five_hour") {
        return None;
    }
    let usage = u
        .iter()
        .map(|(k, v)| Limit {
            key: k.clone(),
            pct: g(v, "pct").as_f64().unwrap_or(0.0),
            resets_at: parse_time(g(v, "resets_at")),
            label: g(v, "label").as_str().unwrap_or(k).to_string(),
            weekly: truthy(g(v, "weekly")),
            note: g(v, "note").as_str().unwrap_or("").to_string(),
        })
        .collect();
    let cr = g(&d, "credits");
    let reading = Reading {
        usage,
        plan: g(&d, "plan").as_str().unwrap_or("").to_string(),
        credits: if truthy(cr) { Some(credits_from_json(cr)) } else { None },
        source: g(&d, "source").as_str().unwrap_or("").to_string(),
    };
    Some((reading, g(&d, "t").as_f64().unwrap_or(0.0)))
}

// ----------------------------------------------------------------- HTTP

struct Resp {
    status: u16,
    body: String,
    retry_after: Option<String>,
}

impl Resp {
    fn ok(&self) -> bool {
        self.status < 400
    }
    fn json(&self) -> Result<Value, FetchError> {
        serde_json::from_str(&self.body).map_err(|_| FetchError::Msg("Unexpected response from the server".into()))
    }
    /// Seconds to wait after a 429.
    fn retry_after_secs(&self) -> u64 {
        match self.retry_after.as_deref().map(|s| s.trim().parse::<f64>()) {
            None => 300,
            Some(Ok(f)) => (f as i64).max(60) as u64,
            Some(Err(_)) => 300,
        }
    }
}

fn agent(timeout_secs: u64) -> ureq::Agent {
    let mut b = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(timeout_secs));
    if let Ok(tls) = native_tls::TlsConnector::new() {
        b = b.tls_connector(Arc::new(tls));
    }
    b.build()
}

/// Turn a ureq result into status + body. The error text never includes headers.
fn finish(r: Result<ureq::Response, ureq::Error>) -> Result<Resp, String> {
    let resp = match r {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(t)) => return Err(format!("network error: {}", t.kind())),
    };
    let status = resp.status();
    let retry_after = resp.header("Retry-After").map(|s| s.to_string());
    let body = resp.into_string().unwrap_or_default();
    Ok(Resp { status, body, retry_after })
}

fn http_get(a: &ureq::Agent, url: &str, headers: &[(&str, &str)]) -> Result<Resp, String> {
    let mut req = a.get(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    finish(req.call())
}

// ----------------------------------------------------------------- Claude Code login

fn creds_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default();
    home.join(".claude").join(".credentials.json")
}

/// Refresh Claude Code's expired login and write it back so Claude Code keeps working.
fn refresh_claude_code_token(data: &mut Value) -> Result<String, FetchError> {
    let path = creds_path();
    let backup = PathBuf::from(format!("{}.bak-claudeusage", path.display()));
    if !backup.exists() {
        let _ = std::fs::copy(&path, &backup);
    }
    let refresh = g(g(data, "claudeAiOauth"), "refreshToken").as_str().unwrap_or("").to_string();
    let a = agent(20);
    let mut last = String::new();
    for url in OAUTH_TOKEN_URLS {
        let body = json!({"grant_type": "refresh_token", "refresh_token": refresh, "client_id": OAUTH_CLIENT_ID});
        let r = match finish(a.post(url).send_json(body)) {
            Ok(r) => r,
            Err(e) => {
                last = e;
                continue;
            }
        };
        if r.ok() {
            let j = r.json()?;
            let Some(token) = g(&j, "access_token").as_str().map(|s| s.to_string()) else {
                last = "no token in the answer".into();
                continue;
            };
            let expires_in = g(&j, "expires_in").as_f64().unwrap_or(3600.0);
            let oauth = data.get_mut("claudeAiOauth").and_then(|o| o.as_object_mut()).ok_or_else(|| {
                FetchError::Msg("No Claude Code login found on this PC".into())
            })?;
            oauth.insert("accessToken".into(), json!(token));
            if let Some(rt) = g(&j, "refresh_token").as_str().filter(|s| !s.is_empty()) {
                oauth.insert("refreshToken".into(), json!(rt));
            }
            oauth.insert("expiresAt".into(), json!(((now() + expires_in) * 1000.0) as i64));
            let tmp = PathBuf::from(format!("{}.tmp", path.display()));
            let text = serde_json::to_string(data).unwrap_or_default();
            if std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path)).is_err() {
                log("Couldn't save the renewed Claude Code login");
            }
            return Ok(token);
        }
        last = format!("HTTP {}", r.status);
    }
    err(format!("Couldn't refresh the Claude Code login ({last}). Run claude and sign in again."))
}

fn fetch_claude_code(settings: &Settings) -> Result<(Vec<Limit>, String, Option<Credits>), FetchError> {
    let mut data = match store::load_json(&creds_path()) {
        Some(d) if g(&d, "claudeAiOauth").is_object() => d,
        _ => return err("No Claude Code login found on this PC"),
    };
    let oauth = g(&data, "claudeAiOauth").clone();
    let mut token = g(&oauth, "accessToken").as_str().unwrap_or("").to_string();
    let expires = g(&oauth, "expiresAt").as_f64().unwrap_or(0.0);
    let allow_refresh = settings.b("allow_token_refresh");
    if expires / 1000.0 < now() + 60.0 {
        if !allow_refresh {
            return err("Claude Code login expired. Open Claude Code to renew it.");
        }
        token = refresh_claude_code_token(&mut data)?;
    }
    let a = agent(20);
    let get = |t: &str| {
        let auth = format!("Bearer {t}");
        http_get(&a, OAUTH_USAGE_URL, &[("Authorization", &auth), ("anthropic-beta", "oauth-2025-04-20")])
            .map_err(FetchError::Msg)
    };
    let mut r = get(&token)?;
    if r.status == 401 && allow_refresh {
        let mut fresh = store::load_json(&creds_path()).unwrap_or(data);
        token = refresh_claude_code_token(&mut fresh)?;
        r = get(&token)?;
    }
    if r.status == 429 {
        return Err(FetchError::Rate(r.retry_after_secs()));
    }
    if !r.ok() {
        return err(format!("Usage request failed (HTTP {})", r.status));
    }
    let plan = capitalize(g(&oauth, "subscriptionType").as_str().unwrap_or(""));
    let j = r.json()?;
    log_new_fields(&j);
    Ok((parse_usage(&j)?, plan, parse_credits(&j)))
}

// ----------------------------------------------------------------- claude.ai session

struct AiSession {
    agent: ureq::Agent,
    cookie: String,
    org: String,
}

impl AiSession {
    fn get(&self, url: &str) -> Result<Resp, FetchError> {
        http_get(&self.agent, url, &[("User-Agent", BROWSER_UA), ("Accept", "application/json"), ("Cookie", &self.cookie)])
            .map_err(FetchError::Msg)
    }
}

/// Logged-in claude.ai session and the chat organization's id.
fn claude_ai_session(settings: &Settings) -> Result<AiSession, FetchError> {
    let key = settings.session_key();
    if key.is_empty() {
        return err("No claude.ai session key set");
    }
    let mut s = AiSession { agent: agent(20), cookie: format!("sessionKey={key}"), org: String::new() };
    let r = s.get("https://claude.ai/api/organizations")?;
    if !r.ok() {
        return err(format!("claude.ai refused the session key (HTTP {})", r.status));
    }
    let orgs = r.json()?;
    let list = orgs.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return err("No organizations on this claude.ai account");
    }
    let has_chat = |o: &Value| g(o, "capabilities").as_array().map(|c| c.iter().any(|x| x == "chat")).unwrap_or(false);
    let org = list.iter().find(|o| has_chat(o)).unwrap_or(&list[0]);
    s.org = match g(org, "uuid").as_str() {
        Some(u) => u.to_string(),
        None => return err("Unexpected response from claude.ai"),
    };
    Ok(s)
}

/// Best-effort credit balance from a response whose exact shape isn't documented.
fn money_any(obj: &Value) -> Option<Money> {
    match obj {
        Value::Array(a) => a.iter().find_map(money_any),
        Value::Object(o) => {
            if let Some(m) = money(obj, None, 2) {
                return Some(m);
            }
            let cur = o.get("currency").and_then(|c| c.as_str()).filter(|s| !s.is_empty()).unwrap_or("USD");
            for k in ["balance", "remaining_balance", "available_balance", "amount", "credits", "remaining", "total"] {
                match g(obj, k) {
                    v @ (Value::Object(_) | Value::Array(_)) => {
                        if let Some(m) = money_any(v) {
                            return Some(m);
                        }
                    }
                    v @ Value::Number(n) => {
                        let f = n.as_f64()?;
                        if !is_int(v) && f.fract() != 0.0 {
                            return Some(Money { amount: f, currency: cur.into() }); // already whole units, e.g. 6.2
                        }
                        let e = o.get("exponent").and_then(|e| e.as_i64()).unwrap_or(2);
                        return Some(Money { amount: f / 10f64.powi(e as i32), currency: cur.into() });
                    }
                    _ => {}
                }
            }
            None
        }
        _ => None,
    }
}

#[derive(Clone, Debug, Default)]
pub struct AiData {
    pub extras: Vec<Extra>,
    pub balance: Option<Money>,
    pub balance_expires: Option<String>,
}

impl AiData {
    pub fn to_json(&self) -> Value {
        json!({"extras": self.extras.iter().map(extra_to_json).collect::<Vec<_>>(),
               "balance": self.balance.as_ref().map(|m| m.to_json()),
               "balance_expires": self.balance_expires})
    }
    pub fn from_json(v: &Value) -> Option<AiData> {
        if !v.is_object() {
            return None;
        }
        Some(AiData {
            extras: g(v, "extras").as_array().map(|a| a.iter().filter_map(extra_from_json).collect()).unwrap_or_default(),
            balance: Money::from_json(g(v, "balance")),
            balance_expires: g(v, "balance_expires").as_str().map(|s| s.to_string()),
        })
    }
}

struct AiCache {
    t: f64,
    data: Option<AiData>,
    logged: Vec<String>,
}

static AI: Mutex<AiCache> = Mutex::new(AiCache { t: 0.0, data: None, logged: Vec::new() });

fn ai_lock() -> std::sync::MutexGuard<'static, AiCache> {
    AI.lock().unwrap_or_else(|e| e.into_inner())
}

/// Restore the claude.ai cache saved in fetch_state.json.
pub fn set_ai_cache(t: f64, data: Option<AiData>) {
    let mut c = ai_lock();
    c.t = t;
    c.data = data;
}

/// Ask claude.ai on the next refresh.
pub fn reset_ai_cache_time() {
    ai_lock().t = 0.0;
}

/// Limit resets and the credit balance, which only claude.ai reports (read-only).
///
/// Asked at most every 15 minutes.
fn claude_ai_extras(settings: &Settings) -> Result<Option<AiData>, FetchError> {
    {
        let mut c = ai_lock();
        if now() - c.t < 15.0 * 60.0 {
            return Ok(c.data.clone());
        }
        c.t = now();
    }
    let s = claude_ai_session(settings)?;
    let mut data = AiData::default();
    let base = format!("https://claude.ai/api/organizations/{}", s.org);
    let r = s.get(&format!("{base}/usage?at_wall=1&cedar_ember=1"))?;
    if r.ok() {
        let j = r.json()?;
        log_new_fields(&j);
        let c = parse_credits(&j).unwrap_or_default();
        data.extras = c.extras;
        data.balance = c.balance;
    }
    // The credit balance, in a response whose shape isn't documented.
    let path = "/prepaid/credits";
    let r = s.get(&format!("{base}{path}"))?;
    let first = {
        let mut c = ai_lock();
        let new = !c.logged.iter().any(|p| p == path);
        if new {
            c.logged.push(path.to_string());
        }
        new
    };
    if first {
        // record the shape once, to check the numbers are read right
        let body: String = if r.ok() { r.body.chars().take(1500).collect() } else { String::new() };
        log(&format!("claude.ai {path}: HTTP {} {body}", r.status));
    }
    if r.ok() && data.balance.is_none() {
        if let Ok(pj) = r.json() {
            data.balance = money_any(&pj);
            data.balance_expires = parse_time(g(&pj, "next_expires_at")).map(|t| t.to_rfc3339());
        }
    }
    let t = {
        let mut c = ai_lock();
        c.data = Some(data.clone());
        c.t
    };
    store::save_fetch_state(&[("ai_t", json!(t)), ("ai_data", data.to_json())]);
    Ok(Some(data))
}

/// Merge claude.ai's resets and credit balance into a Claude Code result.
fn add_claude_ai_extras(
    (usage, plan, credits): (Vec<Limit>, String, Option<Credits>),
    settings: &Settings,
) -> (Vec<Limit>, String, Option<Credits>) {
    if settings.session_key().is_empty() {
        return (usage, plan, credits);
    }
    let d = match claude_ai_extras(settings) {
        Ok(Some(d)) => d,
        Ok(None) => return (usage, plan, credits),
        Err(e) => {
            log(&format!("claude.ai extras failed: {e}"));
            return (usage, plan, credits);
        }
    };
    let mut c = credits.unwrap_or_default();
    let keys: HashSet<String> = c.extras.iter().map(|x| x.key.clone()).collect();
    c.extras.extend(d.extras.into_iter().filter(|x| !keys.contains(&x.key)));
    if d.balance.is_some() && c.balance.is_none() {
        c.balance = d.balance;
        c.balance_expires = d.balance_expires.as_deref().and_then(parse_time_str);
    }
    (usage, plan, Some(c))
}

fn fetch_claude_ai(settings: &Settings) -> Result<(Vec<Limit>, String, Option<Credits>), FetchError> {
    let s = claude_ai_session(settings)?;
    let r = s.get(&format!("https://claude.ai/api/organizations/{}/usage", s.org))?;
    if r.status == 429 {
        return Err(FetchError::Rate(r.retry_after_secs()));
    }
    if !r.ok() {
        return err(format!("claude.ai usage request failed (HTTP {})", r.status));
    }
    let j = r.json()?;
    log_new_fields(&j);
    Ok(add_claude_ai_extras((parse_usage(&j)?, String::new(), parse_credits(&j)), settings))
}

// ----------------------------------------------------------------- update check

/// 'v1.0.10' -> [1, 0, 10]; non-numeric parts count as 0.
pub fn version_tuple(v: &str) -> Vec<u64> {
    v.trim().trim_start_matches(['v', 'V']).split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// Version of the newest GitHub release, or None if it can't be found out.
pub fn latest_release_version() -> Result<Option<String>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let ua = format!("{}/{}", store::APP_NAME, store::VERSION);
    let r = http_get(&agent(10), &url, &[("Accept", "application/vnd.github+json"), ("User-Agent", &ua)])?;
    if r.status != 200 {
        return Ok(None);
    }
    let j: Value = serde_json::from_str(&r.body).map_err(|_| "bad JSON".to_string())?;
    let tag = g(&j, "tag_name").as_str().unwrap_or("").trim_start_matches(['v', 'V']).to_string();
    Ok(if tag.is_empty() { None } else { Some(tag) })
}

// ----------------------------------------------------------------- demo and entry point

fn fetch_demo() -> Reading {
    let n = Utc::now();
    let m = (now() / 30.0) % 100.0;
    let week = n + Duration::days(4) + Duration::hours(3);
    let lim = |key: &str, pct: f64, at, label: &str, weekly, note: &str| Limit {
        key: key.into(), pct, resets_at: Some(at), label: label.into(), weekly, note: note.into(),
    };
    let usd = |a: f64| Some(Money { amount: a, currency: "USD".into() });
    Reading {
        usage: vec![
            lim("five_hour", m.round(), n + Duration::hours(2) + Duration::minutes(14), "5-hour session", false, ""),
            lim("seven_day", 10.0, week, "Weekly, all models", true, ""),
            lim("weekly_fable", 4.0, week, "Weekly, Fable", true, "Can use up to 50% of your weekly limit"),
        ],
        plan: "Max (demo)".into(),
        credits: Some(Credits {
            enabled: true,
            used: usd(12.40),
            limit: usd(50.0),
            balance: usd(37.60),
            balance_expires: Some(n + Duration::days(40)),
            promo: usd(5.0),
            promo_expires: Some(n + Duration::days(6)),
            limit_reached: false,
            extras: vec![Extra {
                key: "demo_reset".into(),
                name: "Limit reset".into(),
                amount: None,
                count: Some(json!(1)),
                expires: Some(n + Duration::days(3)),
            }],
        }),
        source: "Demo".into(),
    }
}

pub fn fetch(settings: &Settings) -> Result<Reading, FetchError> {
    if store::demo() {
        return Ok(fetch_demo());
    }
    let src = settings.s("source");
    let has_key = !settings.session_key().is_empty();
    let mut errors: Vec<String> = Vec::new();
    let wrap = |(usage, plan, credits): (Vec<Limit>, String, Option<Credits>), source: &str| Reading {
        usage, plan, credits, source: source.into(),
    };
    if src == "auto" || src == "claude_code" {
        match fetch_claude_code(settings) {
            Ok(r) => return Ok(wrap(add_claude_ai_extras(r, settings), "Claude Code login")),
            Err(FetchError::Rate(s)) => {
                if src == "claude_code" || !has_key {
                    return Err(FetchError::Rate(s));
                }
                errors.push("Claude Code login is rate limited".into());
            }
            Err(e) => errors.push(e.to_string()),
        }
    }
    if src == "auto" || src == "claude_ai" {
        match fetch_claude_ai(settings) {
            Ok(r) => return Ok(wrap(r, "claude.ai session")),
            Err(e) => errors.push(e.to_string()),
        }
    }
    err(errors.join(" / "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_limits_list() {
        let j = json!({
            "limits": [
                {"kind": "session", "percent": 42, "resets_at": "2026-11-17T20:00:00Z"},
                {"kind": "weekly_all", "percent": 63.4, "resets_at": "2026-11-20T08:00:00+00:00"},
                {"kind": "weekly_model", "percent": 4, "group": "weekly",
                 "scope": {"model": {"display_name": "Fable"}}}
            ],
            "weekly_scoped_shares": [{"limit_index": 2, "allowance_percent_of_weekly": 50}]
        });
        let u = parse_usage(&j).unwrap();
        assert_eq!(u.len(), 3);
        assert_eq!(u[0].key, "five_hour");
        assert_eq!(u[1].label, "Weekly, all models");
        assert_eq!(u[2].key, "weekly_fable");
        assert!(u[2].weekly);
        assert_eq!(u[2].note, "Can use up to 50% of your weekly limit");
    }

    #[test]
    fn parses_old_format() {
        let j = json!({"five_hour": {"utilization": 12.0, "resets_at": null}, "seven_day": {"utilization": 3}});
        let u = parse_usage(&j).unwrap();
        assert_eq!(u.iter().map(|l| l.key.as_str()).collect::<Vec<_>>(), ["five_hour", "seven_day"]);
        assert!(parse_usage(&json!({})).is_err());
    }

    #[test]
    fn parses_credits_and_extras() {
        let j = json!({
            "five_hour": {"utilization": 1},
            "spend": {"enabled": true, "used": {"amount_minor": 1240, "exponent": 2, "currency": "USD"},
                      "limit": {"amount_minor": 5000, "exponent": 2, "currency": "USD"}},
            "cedar_ember": {"grants": [{"id": "g1", "type": "full", "resets_left": 1, "usable_now": true,
                                        "ends_at": "2026-11-20T00:00:00Z"},
                                       {"id": "g2", "resets_left": 0}]},
            "juniper_tide": {"available": false},
            "some_offer": {"display_name": "Free week", "count": 2},
            "pace": {"x": 1},
            "empty_thing": []
        });
        let c = parse_credits(&j).unwrap();
        assert!(c.enabled);
        assert_eq!(c.used, Some(Money { amount: 12.4, currency: "USD".into() }));
        assert_eq!(c.limit.as_ref().unwrap().amount, 50.0);
        let names: Vec<_> = c.extras.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["Full reset", "Free week"]);
        assert_eq!(c.extras[0].key, "cedar_ember:g1");
        assert_eq!(c.extras[1].count, Some(json!(2)));
    }

    #[test]
    fn versions_compare() {
        assert!(version_tuple("v1.0.10") > version_tuple("1.0.9"));
        assert!(version_tuple("2.0.0") > version_tuple("1.0.12"));
        assert_eq!(version_tuple("v1.2"), vec![1, 2]);
    }

    #[test]
    fn money_any_shapes() {
        assert_eq!(money_any(&json!({"balance": 620, "currency": "EUR"})).unwrap().amount, 6.2);
        assert_eq!(money_any(&json!({"amount": 6.25})).unwrap().amount, 6.25);
        assert_eq!(money_any(&json!([{"remaining": {"amount_minor": 100, "exponent": 2}}])).unwrap().amount, 1.0);
    }
}
