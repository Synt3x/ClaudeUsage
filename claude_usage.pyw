"""Claude Usage - shows your Claude 5-hour usage next to the Windows clock.

Run:  pythonw claude_usage.pyw          (normal)
      python  claude_usage.pyw --demo   (fake data, for trying out the UI)
"""
import ctypes
import ctypes.wintypes as wt
import json
import os
import queue
import shutil
import sys
import threading
import time
import tkinter as tk
import webbrowser
import winreg
from datetime import datetime, timedelta, timezone

import requests
import pystray
from PIL import Image, ImageDraw, ImageFont, ImageTk

APP_NAME = "ClaudeUsage"
__version__ = "1.0.12"  # bump this before each build; build.py reads it
HERE = os.path.dirname(os.path.abspath(__file__))
DATA_DIR = os.path.join(os.environ.get("APPDATA", HERE), APP_NAME)
SETTINGS_PATH = os.path.join(DATA_DIR, "settings.json")
HISTORY_PATH = os.path.join(DATA_DIR, "history.json")
CREDS_PATH = os.path.expanduser("~/.claude/.credentials.json")
FROZEN = getattr(sys, "frozen", False)  # True when built with PyInstaller
DEMO = "--demo" in sys.argv

# Claude Code's public OAuth client id: built into Claude Code and the same for every user.
# It is not a secret; it only identifies the app when renewing an expired login.
OAUTH_CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
OAUTH_TOKEN_URLS = ["https://platform.claude.com/v1/oauth/token",
                    "https://console.anthropic.com/v1/oauth/token"]
OAUTH_USAGE_URL = "https://api.anthropic.com/api/oauth/usage"
# Update check: public GitHub API, no login or token is sent.
REPO = "Synt3x/ClaudeUsage"
LATEST_RELEASE_API = f"https://api.github.com/repos/{REPO}/releases/latest"
RELEASES_URL = f"https://github.com/{REPO}/releases/latest"
USAGE_URL = "https://claude.ai/settings/usage"
BROWSER_UA = ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
              "(KHTML, like Gecko) Chrome/140.0 Safari/537.36")

# Colors (dark mode)
CLAY = "#D97757"
AMBER = "#E8A33D"
RED = "#E5534B"
BG = "#262624"
BG2 = "#30302E"
TRACK = "#3A3A37"
FG = "#EEEEEA"
FG2 = "#C9C9C4"
MUTED = "#9A9A95"
KEY = "#010203"  # transparent key color for the taskbar widget
FONT = "Segoe UI"

WINDOWS = [  # (api key, label, is_weekly)
    ("five_hour", "5-hour session", False),
    ("seven_day", "Weekly, all models", True),
    ("seven_day_opus", "Weekly, Opus", True),
    ("seven_day_sonnet", "Weekly, Sonnet", True),
]

DEFAULTS = {
    "autostart": False,
    "check_updates": True,        # look for a newer release on GitHub at startup
    "show_taskbar_widget": True,
    "show_credits": True,         # popup: credits, resets and the usage page link
    "show_chart": True,           # popup: this week's graph
    "notify_20": True,
    "notify_10": True,
    "notify_5": True,
    "notify_reset": True,         # default for limit types not in notify_reset_keys
    "notify_reset_keys": {},      # per limit type: {"five_hour": True, "weekly_fable": False, ...}
    "notify_weekly": True,
    "refresh_minutes": 5,
    "source": "auto",            # auto | claude_code | claude_ai
    "allow_token_refresh": True,  # refresh Claude Code's login when it has expired
    "session_key": "",            # claude.ai sessionKey cookie
    "widget_square": True,       # small colored square in the taskbar widget
    "widget_outline": True,      # rounded outline around the taskbar widget
    "widget_timer": True,        # reset countdown in the taskbar widget
    "widget_five": True,          # 5-hour usage in the taskbar widget
    "widget_weekly": False,       # weekly usage in the taskbar widget
    "tray_show": "five_hour",     # tray icon number: off | five_hour | seven_day
    "widget_offset": 0,           # px the user dragged the taskbar widget
}


# ----------------------------------------------------------------- storage
def load_json(path, default):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return default


def save_json(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=1)
    os.replace(tmp, path)


# Never ask the server more often than this, also across restarts: the time of the last request
# (and any rate-limit wait) is saved in FETCH_STATE_PATH.
MIN_FETCH_GAP = 5 * 60
FETCH_STATE_PATH = os.path.join(DATA_DIR, "fetch_state.json")


def save_fetch_state(**values):
    if DEMO:
        return
    st = load_json(FETCH_STATE_PATH, {})
    st.update(values)
    save_json(FETCH_STATE_PATH, st)


def load_settings():
    s = dict(DEFAULTS)
    s.update(load_json(SETTINGS_PATH, {}))
    s["refresh_minutes"] = max(MIN_FETCH_GAP // 60, s["refresh_minutes"])
    return s


# ----------------------------------------------------------------- autostart
RUN_KEY = r"Software\Microsoft\Windows\CurrentVersion\Run"


def set_autostart(enabled):
    if FROZEN:  # running as ClaudeUsage.exe
        cmd = f'"{sys.executable}"'
    else:
        exe = sys.executable
        if exe.lower().endswith("python.exe"):
            exe = exe[:-10] + "pythonw.exe"
        cmd = f'"{exe}" "{os.path.abspath(__file__)}"'
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY, 0, winreg.KEY_SET_VALUE) as k:
        if enabled:
            winreg.SetValueEx(k, APP_NAME, 0, winreg.REG_SZ, cmd)
        else:
            try:
                winreg.DeleteValue(k, APP_NAME)
            except FileNotFoundError:
                pass


# ----------------------------------------------------------------- fetching
class UsageError(Exception):
    pass


class RateLimited(UsageError):
    """The usage endpoint answered 429: wait before asking again."""
    def __init__(self, retry_after):
        self.retry_after = retry_after
        super().__init__("rate limited")


def retry_after_secs(r, default=300):
    try:
        return max(60, int(float(r.headers.get("Retry-After", default))))
    except ValueError:
        return default


LAST_USAGE_PATH = os.path.join(DATA_DIR, "last_usage.json")


def save_last_usage(usage, plan, source, credits=None):
    cr = {k: (v.isoformat() if isinstance(v, datetime) else v) for k, v in (credits or {}).items()}
    data = {"t": time.time(), "plan": plan, "source": source, "credits": cr or None,
            "usage": {k: dict(v, resets_at=v["resets_at"].isoformat() if v["resets_at"] else None)
                      for k, v in usage.items()}}
    save_json(LAST_USAGE_PATH, data)


def load_last_usage():
    """Last good reading, so a restart shows numbers right away instead of asking the server again."""
    d = load_json(LAST_USAGE_PATH, None)
    if not d or "usage" not in d or "five_hour" not in d["usage"]:
        return None
    usage = {k: dict(v, resets_at=parse_time(v.get("resets_at"))) for k, v in d["usage"].items()}
    cr = d.get("credits") or None
    if cr:
        cr = {k: (parse_time(v) if k.endswith("_expires") else tuple(v) if isinstance(v, list) else v)
              for k, v in cr.items()}
    return usage, d.get("plan", ""), d.get("source", ""), d.get("t", 0), cr


def parse_time(s):
    try:
        return datetime.fromisoformat(s.replace("Z", "+00:00")) if s else None
    except ValueError:
        return None


def parse_usage(j):
    """Return {key: {pct, resets_at, label, weekly, note}} in display order.

    Prefers the generic "limits" list, which also covers per-model weekly limits
    such as Fable (and any added later). Falls back to the fixed fields.
    """
    out = {}
    shares = {s.get("limit_index"): s for s in (j.get("weekly_scoped_shares") or [])}
    for idx, lim in enumerate(j.get("limits") or []):
        kind, pct = lim.get("kind"), lim.get("percent")
        if pct is None:
            continue
        entry = {"pct": float(pct), "resets_at": parse_time(lim.get("resets_at")), "note": ""}
        if kind == "session":
            key, entry["label"], entry["weekly"] = "five_hour", "5-hour session", False
        elif kind == "weekly_all":
            key, entry["label"], entry["weekly"] = "seven_day", "Weekly, all models", True
        else:
            scope = lim.get("scope") or {}
            name = ((scope.get("model") or {}).get("display_name")
                    or (scope.get("surface") or {}).get("display_name") or kind)
            key = "weekly_" + "".join(ch for ch in name.lower() if ch.isalnum())
            entry["label"], entry["weekly"] = f"Weekly, {name}", lim.get("group") == "weekly"
            share = shares.get(idx)
            if share and share.get("allowance_percent_of_weekly"):
                entry["note"] = f"Can use up to {share['allowance_percent_of_weekly']:.0f}% of your weekly limit"
        out[key] = entry
    if not out:  # older response format
        for key, label, weekly in WINDOWS:
            w = j.get(key)
            if isinstance(w, dict) and w.get("utilization") is not None:
                out[key] = {"pct": float(w["utilization"]), "resets_at": parse_time(w.get("resets_at")),
                            "label": label, "weekly": weekly, "note": ""}
    if "five_hour" not in out:
        raise UsageError("No 5-hour data in the response")
    return out


CURRENCY = {"USD": "$", "EUR": "€", "GBP": "£"}


def fmt_money(amount, currency):
    sym = CURRENCY.get((currency or "USD").upper())
    return f"{sym}{amount:,.2f}" if sym else f"{amount:,.2f} {currency}"


def money(m, currency=None, decimals=2):
    """{"amount_minor": 1234, "exponent": 2, "currency": "USD"} (or a bare minor-unit number) -> (12.34, "USD")."""
    if isinstance(m, dict) and m.get("amount_minor") is not None:
        return m["amount_minor"] / 10 ** (m.get("exponent") or 0), m.get("currency") or currency or "USD"
    if isinstance(m, (int, float)) and not isinstance(m, bool):
        return m / 10 ** decimals, currency or "USD"
    return None


def find_expiry(obj):
    """First expires_at / expiry style timestamp anywhere inside obj."""
    if isinstance(obj, dict):
        for k, v in obj.items():
            if "expir" in k.lower() and isinstance(v, str):
                t = parse_time(v)
                if t:
                    return t
        for v in obj.values():
            t = find_expiry(v)
            if t:
                return t
    elif isinstance(obj, list):
        for v in obj:
            t = find_expiry(v)
            if t:
                return t
    return None


def parse_credits(j):
    """Usage credits (extra usage) and promo credits, or None when the response has none."""
    sp = j.get("spend") or {}
    ex = j.get("extra_usage") or {}
    if not sp and not ex:
        return None
    cur = ex.get("currency")
    dec = ex.get("decimal_places") if ex.get("decimal_places") is not None else 2
    used = money(sp.get("used")) or money(ex.get("used_credits"), cur, dec)
    limit = money(sp.get("limit")) or money(sp.get("cap")) or money(ex.get("monthly_limit"), cur, dec)
    balance = money(sp.get("balance"))
    promo = j.get("promo_credits")
    promo_amt = money(promo) or (money(promo.get("balance") or promo.get("remaining") or promo.get("amount"))
                                 if isinstance(promo, dict) else None)
    return {
        "extras": parse_extras(j),
        "enabled": bool(sp.get("enabled") or ex.get("is_enabled")),
        "used": used, "limit": limit, "balance": balance,
        "balance_expires": find_expiry(sp.get("balance")),
        "promo": promo_amt, "promo_expires": find_expiry(promo),
        "limit_reached": bool(ex.get("spend_limit_reached")),
    }


# Fields of the usage response the app knows about. Anything else that turns up non-empty
# (for example a reset you can use) is written to app.log so it can be added later.
KNOWN_FIELDS = {"five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet", "limits", "spend",
                "extra_usage", "promo_credits", "weekly_scoped_shares", "seven_day_breakdown",
                "member_dashboard_available"}
_logged_fields = set()


INFO_FIELDS = {"pace", "indicator"}  # informational, not something you can use
# Limit-reset programs (asked for with ?at_wall=1&cedar_ember=1); parsed on their own below.
RESET_FIELDS = {"juniper_tide", "cedar_ember"}


def parse_resets(j):
    """Limit resets you can use right now, from the two reset programs in the usage response."""
    out = []
    jt = j.get("juniper_tide")
    if isinstance(jt, dict) and jt.get("available"):
        exp = parse_time(jt.get("weekly_resets_at")) or find_expiry(jt)
        out.append({"key": "juniper_tide", "name": "Weekly limit reset", "amount": None, "count": None,
                    "expires": exp.isoformat() if exp else None})
    # cedar_ember: reset grants, e.g. {"id": "...", "type": "full", "resets_left": 1,
    # "ends_at": "<timestamp>", "usable_now": true, "label": "One full limit reset"}.
    ce = j.get("cedar_ember")
    for g in (ce.get("grants") or []) if isinstance(ce, dict) else []:
        left = g.get("resets_left")
        if not isinstance(g, dict) or not left or g.get("paused"):
            continue  # used up or paused
        name = "Full reset" if g.get("type") == "full" else (g.get("label") or "Limit reset")
        if not g.get("usable_now"):
            name += " (not usable yet)"
        exp = parse_time(g.get("ends_at")) or find_expiry(g)
        out.append({"key": f"cedar_ember:{g.get('id', '')}", "name": name, "amount": None,
                    "count": left, "expires": exp.isoformat() if exp else None})
    return out


def parse_extras(j):
    """Anything else on the account that could be used: resets, offers, promos, credits.

    The usage response has many unnamed fields that are normally empty. When one of them is
    filled (and isn't just another usage window), it's shown as an extra with whatever amount,
    count and expiry it carries.
    """
    out = parse_resets(j)
    items = [(k, v) for k, v in j.items()
             if k not in KNOWN_FIELDS and k not in INFO_FIELDS and k not in RESET_FIELDS]
    if j.get("promo_credits") not in (None, [], {}, False):
        items.append(("promo_credits", j["promo_credits"]))
    for k, v in items:
        if v in (None, [], {}, False, 0, ""):
            continue
        if isinstance(v, dict) and "utilization" in v:  # a usage window; the limits list covers it
            continue
        if isinstance(v, dict) and v.get("is_enabled") is False and v.get("enabled") is not True:
            continue
        if isinstance(v, dict) and (v.get("eligible") is False or v.get("available") is False):
            continue  # a program you're not in, or nothing to use right now
        name = None
        if isinstance(v, dict):
            name = v.get("display_name") or v.get("title") or v.get("name") or v.get("label")
        name = name if isinstance(name, str) and name else k.replace("_", " ").capitalize()
        amt = money(v) if isinstance(v, dict) else None
        if isinstance(v, dict) and not amt:
            amt = money(v.get("balance") or v.get("remaining") or v.get("amount"))
        count = None
        if isinstance(v, dict):
            for ck in ("count", "remaining_count", "available", "quantity", "resets_remaining", "remaining"):
                if isinstance(v.get(ck), int) and not isinstance(v.get(ck), bool):
                    count = v[ck]
                    break
        elif isinstance(v, int) and not isinstance(v, bool):
            count = v
        elif isinstance(v, list):
            count = len(v)
        exp = find_expiry(v)
        out.append({"key": k, "name": name, "amount": list(amt) if amt else None, "count": count,
                    "expires": exp.isoformat() if exp else None})
    return out


def log_new_fields(j):
    for k, v in j.items():
        if k not in KNOWN_FIELDS and v not in (None, [], {}, False) and k not in _logged_fields:
            _logged_fields.add(k)
            log(f"New usage field '{k}': {json.dumps(v, default=str)[:1500]}")


def refresh_claude_code_token(data):
    """Refresh Claude Code's expired login and write it back so Claude Code keeps working."""
    oauth = data["claudeAiOauth"]
    backup = CREDS_PATH + ".bak-claudeusage"
    if not os.path.exists(backup):
        shutil.copy2(CREDS_PATH, backup)
    last = None
    for url in OAUTH_TOKEN_URLS:
        try:
            r = requests.post(url, json={"grant_type": "refresh_token",
                                         "refresh_token": oauth["refreshToken"],
                                         "client_id": OAUTH_CLIENT_ID}, timeout=20)
        except requests.RequestException as e:
            last = str(e)
            continue
        if r.ok:
            j = r.json()
            oauth["accessToken"] = j["access_token"]
            if j.get("refresh_token"):
                oauth["refreshToken"] = j["refresh_token"]
            oauth["expiresAt"] = int((time.time() + j.get("expires_in", 3600)) * 1000)
            tmp = CREDS_PATH + ".tmp"
            with open(tmp, "w", encoding="utf-8") as f:
                json.dump(data, f)
            os.replace(tmp, CREDS_PATH)
            return oauth["accessToken"]
        last = f"HTTP {r.status_code}"
    raise UsageError(f"Couldn't refresh the Claude Code login ({last}). Run claude and sign in again.")


def fetch_claude_code(settings):
    data = load_json(CREDS_PATH, None)
    if not data or "claudeAiOauth" not in data:
        raise UsageError("No Claude Code login found on this PC")
    oauth = data["claudeAiOauth"]
    token = oauth["accessToken"]
    if oauth.get("expiresAt", 0) / 1000 < time.time() + 60:
        if not settings["allow_token_refresh"]:
            raise UsageError("Claude Code login expired. Open Claude Code to renew it.")
        token = refresh_claude_code_token(data)
    r = requests.get(OAUTH_USAGE_URL, timeout=20, headers={
        "Authorization": "Bearer " + token, "anthropic-beta": "oauth-2025-04-20"})
    if r.status_code == 401 and settings["allow_token_refresh"]:
        token = refresh_claude_code_token(load_json(CREDS_PATH, data))
        r = requests.get(OAUTH_USAGE_URL, timeout=20, headers={
            "Authorization": "Bearer " + token, "anthropic-beta": "oauth-2025-04-20"})
    if r.status_code == 429:
        raise RateLimited(retry_after_secs(r))
    if not r.ok:
        raise UsageError(f"Usage request failed (HTTP {r.status_code})")
    plan = (oauth.get("subscriptionType") or "").capitalize()
    j = r.json()
    log_new_fields(j)
    return parse_usage(j), plan, parse_credits(j)


def claude_ai_session(settings):
    """Logged-in claude.ai session and the chat organization's id."""
    key = settings["session_key"].strip()
    if not key:
        raise UsageError("No claude.ai session key set")
    s = requests.Session()
    s.headers.update({"User-Agent": BROWSER_UA, "Accept": "application/json"})
    s.cookies.set("sessionKey", key, domain="claude.ai")
    r = s.get("https://claude.ai/api/organizations", timeout=20)
    if not r.ok:
        raise UsageError(f"claude.ai refused the session key (HTTP {r.status_code})")
    orgs = r.json()
    if not orgs:
        raise UsageError("No organizations on this claude.ai account")
    org = next((o for o in orgs if "chat" in (o.get("capabilities") or [])), orgs[0])
    return s, org["uuid"]


def money_any(obj):
    """Best-effort credit balance from a response whose exact shape isn't documented."""
    if isinstance(obj, list):
        for v in obj:
            m = money_any(v)
            if m:
                return m
        return None
    if not isinstance(obj, dict):
        return None
    m = money(obj)
    if m:
        return m
    cur = obj.get("currency")
    for k in ("balance", "remaining_balance", "available_balance", "amount", "credits", "remaining", "total"):
        v = obj.get(k)
        if isinstance(v, (dict, list)):
            m = money_any(v)
            if m:
                return m
        elif isinstance(v, (int, float)) and not isinstance(v, bool):
            if isinstance(v, float) and v != int(v):
                return v, cur or "USD"  # already in whole units, e.g. 6.2
            return v / 10 ** (obj.get("exponent") if obj.get("exponent") is not None else 2), cur or "USD"
    return None


_ai_extras = {"t": 0, "data": None}
_ai_logged = set()


def claude_ai_extras(settings):
    """Limit resets and the credit balance, which only claude.ai reports (read-only).

    Asked at most every 15 minutes. Returns {"extras": [...], "balance": (amount, cur) | None}.
    """
    if time.time() - _ai_extras["t"] < 15 * 60:
        return _ai_extras["data"]
    _ai_extras["t"] = time.time()
    s, org = claude_ai_session(settings)
    data = {"extras": [], "balance": None}
    base = f"https://claude.ai/api/organizations/{org}"
    r = s.get(base + "/usage?at_wall=1&cedar_ember=1", timeout=20)
    if r.ok:
        j = r.json()
        log_new_fields(j)
        c = parse_credits(j) or {}
        data["extras"] = c.get("extras") or []
        data["balance"] = c.get("balance")
    for path in ("/prepaid/credits",):
        r = s.get(base + path, timeout=20)
        if path not in _ai_logged:  # record the shape once, to check the numbers are read right
            _ai_logged.add(path)
            log(f"claude.ai {path}: HTTP {r.status_code} {r.text[:1500] if r.ok else ''}")
        if r.ok and not data["balance"]:
            try:
                pj = r.json()
                data["balance"] = money_any(pj)
                exp = parse_time(pj.get("next_expires_at")) if isinstance(pj, dict) else None
                data["balance_expires"] = exp.isoformat() if exp else None
            except ValueError:
                pass
    _ai_extras["data"] = data
    save_fetch_state(ai_t=_ai_extras["t"], ai_data=data)
    return data


def add_claude_ai_extras(result, settings):
    """Merge claude.ai's resets and credit balance into a Claude Code result."""
    usage, plan, credits = result
    if not settings["session_key"].strip():
        return result
    try:
        d = claude_ai_extras(settings)
    except Exception as e:
        log(f"claude.ai extras failed: {e}")
        return result
    if not d:
        return result
    credits = dict(credits or {})
    keys = {x["key"] for x in credits.get("extras") or []}
    credits["extras"] = (credits.get("extras") or []) + [x for x in d["extras"] if x["key"] not in keys]
    if d["balance"] and not credits.get("balance"):
        credits["balance"] = d["balance"]
        credits["balance_expires"] = parse_time(d.get("balance_expires"))
    return usage, plan, credits


def fetch_claude_ai(settings):
    s, org = claude_ai_session(settings)
    r = s.get(f"https://claude.ai/api/organizations/{org}/usage", timeout=20)
    if r.status_code == 429:
        raise RateLimited(retry_after_secs(r))
    if not r.ok:
        raise UsageError(f"claude.ai usage request failed (HTTP {r.status_code})")
    j = r.json()
    log_new_fields(j)
    return add_claude_ai_extras((parse_usage(j), "", parse_credits(j)), settings)


def version_tuple(v):
    """'v1.0.10' -> (1, 0, 10); non-numeric parts count as 0."""
    return tuple(int(p) if p.isdigit() else 0 for p in v.strip().lstrip("vV").split("."))


def latest_release_version():
    """Version of the newest GitHub release, or None if it can't be found out."""
    r = requests.get(LATEST_RELEASE_API, timeout=10, headers={
        "Accept": "application/vnd.github+json", "User-Agent": f"{APP_NAME}/{__version__}"})
    if r.status_code != 200:
        return None
    tag = r.json().get("tag_name") or ""
    return tag.lstrip("vV") or None


def fetch_demo(_settings):
    now = datetime.now(timezone.utc)
    m = (time.time() / 30) % 100
    week = now + timedelta(days=4, hours=3)
    return {
        "five_hour": {"pct": round(m), "resets_at": now + timedelta(hours=2, minutes=14),
                      "label": "5-hour session", "weekly": False, "note": ""},
        "seven_day": {"pct": 10.0, "resets_at": week, "label": "Weekly, all models", "weekly": True, "note": ""},
        "weekly_fable": {"pct": 4.0, "resets_at": week, "label": "Weekly, Fable", "weekly": True,
                         "note": "Can use up to 50% of your weekly limit"},
    }, "Max (demo)", {"enabled": True, "used": (12.40, "USD"), "limit": (50.0, "USD"),
                      "balance": (37.60, "USD"), "balance_expires": now + timedelta(days=40),
                      "promo": (5.0, "USD"), "promo_expires": now + timedelta(days=6), "limit_reached": False,
                      "extras": [{"key": "demo_reset", "name": "Limit reset", "amount": None, "count": 1,
                                  "expires": (now + timedelta(days=3)).isoformat()}]}


def fetch(settings):
    if DEMO:
        return fetch_demo(settings) + ("Demo",)
    src = settings["source"]
    errors = []
    if src in ("auto", "claude_code"):
        try:
            return add_claude_ai_extras(fetch_claude_code(settings), settings) + ("Claude Code login",)
        except RateLimited:
            if src == "claude_code" or not settings["session_key"].strip():
                raise
            errors.append("Claude Code login is rate limited")
        except Exception as e:
            errors.append(str(e))
    if src in ("auto", "claude_ai"):
        try:
            return fetch_claude_ai(settings) + ("claude.ai session",)
        except Exception as e:
            errors.append(str(e))
    raise UsageError(" / ".join(errors))


# ----------------------------------------------------------------- win32 helpers
user32 = ctypes.windll.user32
# Declare handle types: without this, HWND_TOPMOST (-1) is passed as a 32-bit int on 64-bit
# Windows, the call fails, and the widget can end up hidden behind the taskbar
# (for example after Win+Shift+S).
user32.SetWindowPos.argtypes = [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_uint]
user32.SetWindowPos.restype = wt.BOOL
HWND_TOPMOST = -1
# NOSIZE | NOMOVE | NOACTIVATE | NOOWNERZORDER. The last one matters: the widget is owned by the
# taskbar, and without it every raise also lifts the taskbar above other apps' tray menus.
SWP_FLAGS = 0x0001 | 0x0002 | 0x0010 | 0x0200

# Low-level mouse hook, used only while the popup is open so a click outside closes it.
HOOKPROC = ctypes.WINFUNCTYPE(ctypes.c_ssize_t, ctypes.c_int, wt.WPARAM, wt.LPARAM)


class MSLLHOOKSTRUCT(ctypes.Structure):
    _fields_ = [("pt", wt.POINT), ("mouseData", wt.DWORD), ("flags", wt.DWORD),
                ("time", wt.DWORD), ("dwExtraInfo", ctypes.c_size_t)]


user32.SetWindowsHookExW.argtypes = [ctypes.c_int, HOOKPROC, ctypes.c_void_p, wt.DWORD]
user32.SetWindowsHookExW.restype = ctypes.c_void_p
user32.CallNextHookEx.argtypes = [ctypes.c_void_p, ctypes.c_int, wt.WPARAM, wt.LPARAM]
user32.CallNextHookEx.restype = ctypes.c_ssize_t
user32.UnhookWindowsHookEx.argtypes = [ctypes.c_void_p]
ctypes.windll.kernel32.GetModuleHandleW.restype = ctypes.c_void_p
WM_BUTTONDOWNS = (0x0201, 0x0204, 0x0207)  # left, right, middle


def hwnd_of(win):
    return user32.GetAncestor(win.winfo_id(), 2)  # GA_ROOT


def taskbar_anchor():
    """Return (right_edge_x, top_y, height) of the area just left of the tray, or None."""
    tray = user32.FindWindowW("Shell_TrayWnd", None)
    if not tray:
        return None
    rc = wt.RECT()
    user32.GetWindowRect(tray, ctypes.byref(rc))
    if rc.bottom - rc.top > rc.right - rc.left:  # vertical taskbar: not supported, use tray icon
        return None
    notify = user32.FindWindowExW(tray, 0, "TrayNotifyWnd", None)
    right = rc.right - int(300 * dpi_scale())
    if notify:
        nr = wt.RECT()
        user32.GetWindowRect(notify, ctypes.byref(nr))
        if nr.right > nr.left:
            right = nr.left
    return right, rc.top, rc.bottom - rc.top


def work_area():
    rc = wt.RECT()
    user32.SystemParametersInfoW(0x0030, 0, ctypes.byref(rc), 0)  # SPI_GETWORKAREA
    return rc.left, rc.top, rc.right, rc.bottom


def dpi_scale():
    try:
        return user32.GetDpiForSystem() / 96.0
    except Exception:
        return 1.0


# ----------------------------------------------------------------- helpers
def color_for(pct_used):
    left = 100 - pct_used
    if left <= 5:
        return RED
    if left <= 20:
        return AMBER
    return CLAY


def fmt_reset(dt):
    if not dt:
        return ""
    delta = dt - datetime.now(timezone.utc)
    secs = max(0, int(delta.total_seconds()))
    local = dt.astimezone()
    if secs < 24 * 3600:
        h, m = divmod(secs // 60, 60)
        return f"Resets in {h} h {m} min ({local:%H:%M})"
    return f"Resets {local:%a %H:%M}"


def short_name(key, label):
    """Short chip name for a limit: "5-hour", "Weekly", "Fable"."""
    if key == "five_hour":
        return "5-hour"
    if key == "seven_day":
        return "Weekly"
    return label.split(",", 1)[-1].strip() if "," in label else label


def limit_name(key, label):
    """Name used in notifications: "5-hour limit", "weekly limit", "weekly Fable limit"."""
    if key == "five_hour":
        return "5-hour limit"
    if key == "seven_day":
        return "weekly limit"
    return f"weekly {short_name(key, label)} limit"


def short_left(dt):
    """Compact time until dt: "45m", "3h 57m", "2d 5h"."""
    if not dt:
        return ""
    mins = max(0, int((dt - datetime.now(timezone.utc)).total_seconds() // 60))
    if mins >= 24 * 60:
        return f"{mins // 1440}d {mins % 1440 // 60}h"
    return f"{mins // 60}h {mins % 60}m" if mins >= 60 else f"{mins}m"


SEVERITY = {RED: 3, AMBER: 2, CLAY: 1}


def draw_logo(size):
    """The app logo: orange rounded square with two dark blocks, on a transparent background."""
    ss = 4
    S = size * ss
    im = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    m = S * 0.03
    d.rounded_rectangle((m, m, S - 1 - m, S - 1 - m), radius=S * 0.2, fill=CLAY)
    b, g = S * 0.2, S * 0.06  # block size, gap between blocks
    x0, y0 = (S - (2 * b + g)) / 2, (S - b) / 2
    for x in (x0, x0 + b + g):
        d.rounded_rectangle((x, y0, x + b, y0 + b), radius=S * 0.02, fill=(44, 18, 8))
    return im.resize((size, size), Image.LANCZOS)


# ----------------------------------------------------------------- app
class App:
    def __init__(self):
        try:
            ctypes.windll.shcore.SetProcessDpiAwareness(2)
        except Exception:
            pass
        try:  # dark native menus (uxtheme SetPreferredAppMode=ForceDark, FlushMenuThemes)
            ux = ctypes.WinDLL("uxtheme")
            ux[135](2)
            ux[136]()
        except Exception:
            pass
        self.settings = load_settings()
        if self.settings["autostart"]:
            try:  # keep the startup entry pointing at whatever is running now (script or exe)
                set_autostart(True)
            except OSError:
                pass
        self.history = load_json(HISTORY_PATH, [])
        if DEMO:
            import random
            now = time.time()
            self.history = [{"t": int(now - d * 86400), "fh": random.choice([0, 20, 45, 70, 90, 100]),
                             "sd": (30 - d) % 7 * 12} for d in range(30, 0, -1) if d % 5]
        self._drag = (0, 0, False)
        self.notified = load_json(os.path.join(DATA_DIR, "notified.json"), {})
        self.usage = None
        self.credits = None
        self.plan = ""
        self.source = ""
        self.error = None
        self.updated = None
        self.q = queue.Queue()
        self.fetching = False
        self.next_fetch = 0
        self.popup_opened_at = 0
        self.new_version = None  # set when GitHub has a newer release

        self.root = tk.Tk()
        # Errors inside Tk callbacks don't stop the app; record them so problems can be traced.
        self.root.report_callback_exception = lambda t, e, tb: log_exception("UI error:", t, e, tb)
        self.root.withdraw()
        self.scale = dpi_scale()
        self._imgs = {}
        from tkinter import font as tkfont
        fams = set(tkfont.families(self.root))
        self.icon_font = "Segoe Fluent Icons" if "Segoe Fluent Icons" in fams else "Segoe MDL2 Assets"

        self.build_widget()
        self.build_popup()
        self.build_tray()

        self.root.after(200, self.pump)
        self.root.after(1000, self.tick)
        self.root.after(300, self.keep_on_top)
        self.root.after(100, self.watch_outside_click)
        cached = None if DEMO else load_last_usage()
        if cached:
            self.usage, self.plan, self.source, self.updated, self.credits = cached
            self.update_widget()
        st = {} if DEMO else load_json(FETCH_STATE_PATH, {})
        self.last_fetch = st.get("last_fetch", 0)
        self.rate_until = st.get("rate_until", 0)
        ai = st.get("ai_data")
        if ai and ai.get("balance"):
            ai["balance"] = tuple(ai["balance"])
        _ai_extras.update(t=st.get("ai_t", 0), data=ai)
        # After a restart, wait until the regular interval since the last request has passed.
        self.next_fetch = max(self.last_fetch + self.settings["refresh_minutes"] * 60, self.rate_until)
        if time.time() >= self.next_fetch:
            self.request_fetch()
        if self.settings["check_updates"]:
            self.check_for_update()
        if "--show" in sys.argv:
            self.root.after(1500, lambda: self.show_popup(settings="--settings" in sys.argv))

    def px(self, v):
        return int(v * self.scale)

    # ---------------- taskbar widget
    def build_widget(self):
        w = self.widget = tk.Toplevel(self.root, bg="#202020")
        w.overrideredirect(True)
        w.attributes("-topmost", True)
        self.w_img = tk.Label(w, bd=0, highlightthickness=0, bg="#202020")
        self.w_img.pack()
        # Bind on the image only: Tk also routes a child's events through its toplevel's
        # bindings, so binding both made every click toggle the popup twice (open + close).
        for el in (self.w_img,):
            el.bind("<ButtonPress-1>", self.w_press)
            el.bind("<B1-Motion>", self.w_drag)
            el.bind("<ButtonRelease-1>", self.w_release)
            el.bind("<ButtonRelease-3>", self.w_menu)
        self.menu = tk.Menu(self.root, tearoff=0, bg=BG2, fg=FG, activebackground=CLAY,
                            activeforeground="#1b1b1b", bd=0)
        self.menu.add_command(label="Open", command=self.toggle_popup)
        self.menu.add_command(label="Refresh now", command=self.manual_refresh)
        self.menu.add_command(label="Settings", command=lambda: self.show_popup(settings=True))
        self.menu.add_separator()
        self.menu.add_command(label="Quit", command=self.quit)
        self.update_widget()

    def place_widget(self):
        if not self.settings["show_taskbar_widget"]:
            self.widget.withdraw()
            return
        if self.widget.state() != "normal":
            self.widget.deiconify()
        self.widget.update_idletasks()
        if not getattr(self, "w_hwnd", None):
            self.w_hwnd = hwnd_of(self.widget)
        anchor = taskbar_anchor()
        tray = user32.FindWindowW("Shell_TrayWnd", None)
        ww, wh = self.widget.winfo_reqwidth(), self.widget.winfo_reqheight()
        if anchor and tray:
            right, top, height = anchor
            x = right - ww - self.px(6)
            y = top + (height - wh) // 2
            # Make the taskbar our owner: Windows always keeps an owned window above its owner,
            # so clicking the taskbar can't cover the widget. (Embedding it as a child doesn't
            # work on Windows 11: the taskbar's composition layer paints over child windows.)
            if getattr(self, "_owner", None) != tray:
                user32.SetWindowLongPtrW.argtypes = [wt.HWND, ctypes.c_int, ctypes.c_void_p]
                user32.SetWindowLongPtrW(self.w_hwnd, -8, tray)  # GWLP_HWNDPARENT = owner
                self._owner = tray
            cur = wt.RECT()
            user32.GetWindowRect(self.w_hwnd, ctypes.byref(cur))
            # Only move when needed; re-placing it every second made it flicker.
            if (cur.left, cur.top) != (x, y):
                self.widget.geometry(f"+{x}+{y}")
        else:
            l, t, r, b = work_area()
            x, y = r - ww - self.px(320) + self.settings["widget_offset"], b - wh - self.px(10)
            if getattr(self, "_w_pos", None) != (x, y):
                self._w_pos = (x, y)
                self.widget.geometry(f"+{x}+{y}")

    def widget_rect(self):
        rc = wt.RECT()
        user32.GetWindowRect(getattr(self, "w_hwnd", None) or hwnd_of(self.widget), ctypes.byref(rc))
        return rc

    def w_press(self, e):
        self._drag = (e.x_root, self.settings["widget_offset"], False)

    def w_drag(self, e):
        return  # the widget is locked next to the tray icons
        x0, off0, _ = self._drag
        if abs(e.x_root - x0) > 4:
            self._drag = (x0, off0, True)
            self.settings["widget_offset"] = off0 + (e.x_root - x0)
            self.place_widget()

    def w_release(self, _e):
        if self._drag[2]:
            save_json(SETTINGS_PATH, self.settings)
        else:
            self.toggle_popup()

    def w_menu(self, e):
        """Native Windows menu: closes on click outside, uses the dark theme."""
        items = [("Open", self.toggle_popup), ("Refresh now", self.manual_refresh),
                 ("Settings", lambda: self.show_popup(settings=True)), None, ("Quit", self.quit)]
        hmenu = user32.CreatePopupMenu()
        for i, it in enumerate(items, start=1):
            if it is None:
                user32.AppendMenuW(hmenu, 0x0800, 0, None)  # MF_SEPARATOR
            else:
                user32.AppendMenuW(hmenu, 0, i, it[0])
        owner = hwnd_of(self.root)
        user32.SetForegroundWindow(owner)  # required so the menu closes when clicking elsewhere
        # TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN
        cmd = user32.TrackPopupMenu(hmenu, 0x0100 | 0x0002 | 0x0020, e.x_root, e.y_root, 0, owner, None)
        user32.PostMessageW(owner, 0, 0, 0)  # WM_NULL, per the TrackPopupMenu docs
        user32.DestroyMenu(hmenu)
        if cmd and items[cmd - 1]:
            items[cmd - 1][1]()

    def taskbar_bg(self):
        """Sample the taskbar color so the pill blends in (works for light and dark taskbars)."""
        anchor = taskbar_anchor()
        if not anchor:
            return (32, 32, 32)
        right, top, _ = anchor
        hdc = user32.GetDC(0)
        c = ctypes.windll.gdi32.GetPixel(hdc, right + 2, top + 3)
        user32.ReleaseDC(0, hdc)
        if c == -1 or c == 0xFFFFFFFF:
            return (32, 32, 32)
        return (c & 0xFF, (c >> 8) & 0xFF, (c >> 16) & 0xFF)

    def pill_image(self, segs):
        """Taskbar pill. segs: [(label, pct_text, timer_text, color)]; label is "" when only one is shown."""
        bg = self.taskbar_bg()
        ss = 4  # supersample for a smooth rounded outline
        h = self.px(26)
        pad, gap, small, sep = self.px(8), self.px(6), self.px(4), self.px(14)
        try:
            bold = ImageFont.truetype("segoeuib.ttf", self.px(12))
            reg = ImageFont.truetype("segoeui.ttf", self.px(12))
        except OSError:
            bold = reg = ImageFont.load_default()
        st = self.settings
        col = max((seg[3] for seg in segs), key=lambda c: SEVERITY.get(c, 0))  # most urgent color
        sq_w, sq_h = (self.px(12), self.px(10)) if st["widget_square"] else (0, 0)
        probe = ImageDraw.Draw(Image.new("RGB", (1, 1)))
        parts, x = [], pad + sq_w + (gap if sq_w else 0)
        for i, (label, text, timer, c) in enumerate(segs):
            if i:
                parts.append(("sep", x + sep / 2))
                x += sep
            if label:
                parts.append(("label", x, label))
                x += probe.textlength(label, font=reg) + small
            parts.append(("pct", x, text, c))
            x += probe.textlength(text, font=bold)
            if timer:
                x += gap
                parts.append(("timer", x, timer))
                x += probe.textlength(timer, font=reg)
        w = int(x + pad)
        big = Image.new("RGB", (w * ss, h * ss), bg)
        bd = ImageDraw.Draw(big)
        if st["widget_outline"]:
            bd.rounded_rectangle((0, 0, w * ss - 1, h * ss - 1), radius=self.px(6) * ss,
                                 outline=col, width=max(1, round(self.scale)) * ss)
        if sq_w:
            sy = (h - sq_h) / 2
            bd.rounded_rectangle((pad * ss, sy * ss, (pad + sq_w) * ss, (sy + sq_h) * ss),
                                 radius=self.px(2) * ss, fill=col)
        for part in parts:
            if part[0] == "sep":
                bd.line((part[1] * ss, h * 0.3 * ss, part[1] * ss, h * 0.7 * ss), fill=MUTED,
                        width=max(1, round(self.scale)) * ss)
        img = big.resize((w, h), Image.LANCZOS)
        d = ImageDraw.Draw(img)
        for part in parts:
            if part[0] in ("label", "timer"):
                d.text((part[1], h / 2), part[2], fill=MUTED, font=reg, anchor="lm")
            elif part[0] == "pct":
                d.text((part[1], h / 2), part[2], fill=part[3], font=bold, anchor="lm")
        return img, bg

    def update_widget(self):
        st = self.settings
        if self.usage:
            keys = [k for k, on in (("five_hour", st["widget_five"]), ("seven_day", st["widget_weekly"]))
                    if on and k in self.usage] or ["five_hour"]
            both = len(keys) > 1
            segs = []
            for k in keys:
                u = self.usage[k]
                timer = short_left(u["resets_at"]) if st["widget_timer"] else ""
                segs.append(({"five_hour": "5h", "seven_day": "W"}[k] if both else "",
                             f"{u['pct']:.0f}%", timer, color_for(u["pct"])))
        else:
            segs = [("", "!" if self.error else "--%", "", MUTED)]
        img, bg = self.pill_image(segs)
        self.update_tray()
        hexbg = "#%02x%02x%02x" % bg
        self._imgs["w"] = ImageTk.PhotoImage(img)
        self.w_img.configure(image=self._imgs["w"], bg=hexbg)
        self.widget.configure(bg=hexbg)
        self.place_widget()

    # ---------------- popup
    def build_popup(self):
        p = self.popup = self.new_popup_window()
        p.withdraw()
        self.inner = tk.Frame(p, bg=BG, padx=self.px(16), pady=self.px(14))
        self.inner.pack(fill="both", expand=True, padx=1, pady=1)
        self.view = "main"

    def new_popup_window(self):
        p = tk.Toplevel(self.root, bg=TRACK)
        p.overrideredirect(True)
        p.attributes("-topmost", True)
        p.bind("<Escape>", lambda e: self.hide_popup())
        return p

    def clear_inner(self):
        """Build the next page in a new, fully transparent window; position_popup() swaps it in.

        Rebuilding inside the visible window made the popup flash empty for a moment.
        """
        self.popup_opened_at = time.time()  # clicks that started this page change aren't "outside"
        if getattr(self, "_old_popup", None) is None:
            self._old_popup = self.popup
            # Keep the images the visible page uses alive until it's gone; otherwise its
            # bars and chart go blank as soon as the new page replaces them.
            self._old_imgs = dict(self._imgs)
        else:  # a render was started but never shown; drop it
            self.popup.destroy()
        self.popup = self.new_popup_window()
        self.popup.attributes("-alpha", 0.0)
        self.inner = tk.Frame(self.popup, bg=BG, padx=self.px(16), pady=self.px(14))
        self.inner.pack(fill="both", expand=True, padx=1, pady=1)

    @staticmethod
    def rgb(hex_color):
        return tuple(int(hex_color[i:i + 2], 16) for i in (1, 3, 5))

    def logo_image(self, size):
        """Orange rounded square with two dark blocks, as in the sketch."""
        ss = 4
        s = size * ss
        im = Image.new("RGB", (s, s), self.rgb(BG))
        d = ImageDraw.Draw(im)
        d.rounded_rectangle((0, 0, s - 1, s - 1), radius=s * 0.2, fill=self.rgb(CLAY))
        b = s * 0.2  # block size
        g = s * 0.06  # gap between blocks
        x0, y0 = (s - (2 * b + g)) / 2, (s - b) / 2
        for x in (x0, x0 + b + g):
            d.rounded_rectangle((x, y0, x + b, y0 + b), radius=s * 0.02, fill=(44, 18, 8))
        return im.resize((size, size), Image.LANCZOS)

    def bar_image(self, w, h, pct, col):
        """Rounded progress bar: pill-shaped track with a pill-shaped fill."""
        ss = 4
        im = Image.new("RGB", (w * ss, h * ss), self.rgb(BG))
        d = ImageDraw.Draw(im)
        r = h * ss / 2
        d.rounded_rectangle((0, 0, w * ss - 1, h * ss - 1), radius=r, fill=self.rgb(TRACK))
        if pct > 0:
            fw = max(h * ss, w * ss * min(pct, 100) / 100)
            d.rounded_rectangle((0, 0, fw - 1, h * ss - 1), radius=r, fill=self.rgb(col))
        return im.resize((w, h), Image.LANCZOS)

    def header(self, title, sub, buttons):
        h = tk.Frame(self.inner, bg=BG)
        h.pack(fill="x", pady=(0, self.px(12)))
        if "h" not in self._imgs:
            self._imgs["h"] = ImageTk.PhotoImage(self.logo_image(self.px(30)))
        tk.Label(h, image=self._imgs["h"], bg=BG).pack(side="left", padx=(0, self.px(10)))
        t = tk.Frame(h, bg=BG)
        t.pack(side="left")
        tk.Label(t, text=title, bg=BG, fg=FG, font=(FONT, 11, "bold")).pack(anchor="w")
        tk.Label(t, text=sub, bg=BG, fg=MUTED, font=(FONT, 8)).pack(anchor="w")
        for btn in reversed(buttons):
            sym, cmd = btn[0], btn[1]
            disabled = len(btn) > 2 and btn[2]
            # Same icon font for every button, so they line up and match Windows 11 icons.
            b = tk.Label(h, text=sym, bg=BG, fg=TRACK if disabled else MUTED, font=(self.icon_font, 11),
                         cursor="arrow" if disabled else "hand2")
            b.pack(side="right", padx=(self.px(10), 0))
            if disabled:
                continue
            b.bind("<Button-1>", lambda e, c=cmd: c())
            b.bind("<Enter>", lambda e, b=b: b.configure(fg=CLAY))
            b.bind("<Leave>", lambda e, b=b: b.configure(fg=MUTED))

    def bar(self, label, pct, reset, note=""):
        f = tk.Frame(self.inner, bg=BG)
        f.pack(fill="x", pady=(0, self.px(10)))
        row = tk.Frame(f, bg=BG)
        row.pack(fill="x")
        tk.Label(row, text=label, bg=BG, fg=FG2, font=(FONT, 9)).pack(side="left")
        col = color_for(pct)
        tk.Label(row, text=f"{pct:.0f}% used", bg=BG, fg=FG, font=(FONT, 9, "bold")).pack(side="right")
        W, H = self.px(280), self.px(8)
        img = ImageTk.PhotoImage(self.bar_image(W, H, pct, col))
        self._imgs[f"bar_{label}"] = img
        tk.Label(f, image=img, bg=BG, bd=0).pack(anchor="w", pady=(self.px(4), self.px(3)))
        txt = fmt_reset(reset)
        if 100 - pct <= 20:
            txt = f"{100 - pct:.0f}% left · " + txt
        tk.Label(f, text=txt, bg=BG, fg=MUTED, font=(FONT, 8)).pack(anchor="w")
        if note:
            tk.Label(f, text=note, bg=BG, fg=MUTED, font=(FONT, 8)).pack(anchor="w")

    def credits_section(self):
        """Usage credits: what's left, monthly spend limit, promo credits and when they expire."""
        c = self.credits
        if not c:
            return
        head = tk.Frame(self.inner, bg=BG)
        head.pack(fill="x", pady=(self.px(2), self.px(4)))
        tk.Label(head, text="Credits", bg=BG, fg=FG2, font=(FONT, 9)).pack(side="left")
        link = tk.Label(head, text="Open usage page  ↗", bg=BG, fg=CLAY, font=(FONT, 8), cursor="hand2")
        link.pack(side="right")
        link.bind("<Button-1>", lambda e: webbrowser.open(USAGE_URL))

        def line(label, value, value_fg=FG, sub="", sub_fg=MUTED, url=None):
            f = tk.Frame(self.inner, bg=BG)
            f.pack(fill="x", pady=(0, self.px(6)))
            r = tk.Frame(f, bg=BG)
            r.pack(fill="x")
            ws = [f, r, tk.Label(r, text=label, bg=BG, fg=FG2, font=(FONT, 9)),
                  tk.Label(r, text=value, bg=BG, fg=value_fg, font=(FONT, 9, "bold"))]
            ws[2].pack(side="left")
            ws[3].pack(side="right")
            if sub:
                ws.append(tk.Label(f, text=sub, bg=BG, fg=sub_fg, font=(FONT, 8)))
                ws[-1].pack(anchor="w")
            if url:  # whole row opens the page
                for w in ws:
                    w.configure(cursor="hand2")
                    w.bind("<Button-1>", lambda e: webbrowser.open(url))

        def expiry(dt):
            if not dt:
                return "", MUTED
            days = (dt - datetime.now(timezone.utc)).total_seconds() / 86400
            local = dt.astimezone()
            if days < 0:
                return "Expired", RED
            when = f"today {local:%H:%M}" if days < 1 else f"{local:%a %d %b} (in {int(days)} days)"
            return f"Expires {when}", AMBER if days <= 7 else MUTED

        shown = False
        if c.get("balance"):
            line("Credits left", fmt_money(*c["balance"]), FG, *expiry(c.get("balance_expires")))
            shown = True
        if c.get("promo"):
            line("Promo credits", fmt_money(*c["promo"]), FG, *expiry(c.get("promo_expires")))
            shown = True
        if c.get("enabled") and c.get("limit"):
            (used, cur), (limit, _) = c.get("used") or (0.0, c["limit"][1]), c["limit"]
            pct = 100 * used / limit if limit else 0
            f = tk.Frame(self.inner, bg=BG)
            f.pack(fill="x", pady=(0, self.px(6)))
            r = tk.Frame(f, bg=BG)
            r.pack(fill="x")
            tk.Label(r, text="Monthly credit limit", bg=BG, fg=FG2, font=(FONT, 9)).pack(side="left")
            tk.Label(r, text=f"{fmt_money(max(0.0, limit - used), cur)} left", bg=BG, fg=FG,
                     font=(FONT, 9, "bold")).pack(side="right")
            img = ImageTk.PhotoImage(self.bar_image(self.px(280), self.px(8), pct, color_for(pct)))
            self._imgs["bar_credits"] = img
            tk.Label(f, image=img, bg=BG, bd=0).pack(anchor="w", pady=(self.px(4), self.px(3)))
            if c.get("limit_reached"):
                txt, fg = "Monthly limit reached", RED
            else:
                txt, fg = f"{fmt_money(used, cur)} of {fmt_money(limit, cur)} used this month", MUTED
            tk.Label(f, text=txt, bg=BG, fg=fg, font=(FONT, 8)).pack(anchor="w")
            shown = True
        elif c.get("enabled") and c.get("used"):
            line("Credits used", fmt_money(*c["used"]))
            shown = True
        if not shown:
            line("Usage credits", "On" if c.get("enabled") else "Off", FG if c.get("enabled") else MUTED)

        # Resets, offers and promos: shown even when usage credits are off.
        extras = [x for x in (c.get("extras") or []) if x["key"] != "promo_credits" or not c.get("promo")]
        if not extras:
            line("Resets & extras", "None available", MUTED)
        for x in extras:
            if x.get("amount"):
                value = fmt_money(*x["amount"])
            elif x.get("count") is not None:
                value = f"{x['count']} available"
            else:
                value = "Available"
            line(x["name"], value, CLAY, *expiry(parse_time(x.get("expires"))), url=USAGE_URL)

    def history_chart(self):
        """This week's usage: one bar per day of the current weekly period (up to the weekly reset)."""
        wk = self.usage.get("seven_day", {}) if self.usage else {}
        end = wk.get("resets_at") or datetime.now(timezone.utc) + timedelta(days=1)
        start = end - timedelta(days=7)
        tk.Label(self.inner, text=f"This week · until {end.astimezone():%a %H:%M}", bg=BG, fg=FG2,
                 font=(FONT, 9)).pack(anchor="w", pady=(self.px(4), self.px(4)))
        W, H, LBL = self.px(280), self.px(70), self.px(16)
        # One bar per calendar day from the week's start day to its reset day.
        first, last = start.astimezone().date(), end.astimezone().date()
        days = [first + timedelta(days=i) for i in range((last - first).days + 1)]
        n = len(days)
        s0, span = start.timestamp(), 7 * 86400
        peak, line = [0.0] * n, []
        for smp in self.history:
            if s0 <= smp["t"] < s0 + span:
                dt = datetime.fromtimestamp(smp["t"])
                i = (dt.date() - first).days
                if 0 <= i < n:
                    peak[i] = max(peak[i], smp.get("fh") or 0)
                if smp.get("sd") is not None:
                    # x position by the time of day, so the line lines up with the day bars
                    line.append(((i + (dt.hour * 60 + dt.minute) / 1440) / n, smp["sd"]))
        now_i = (datetime.now().date() - first).days

        ss = 4
        im = Image.new("RGB", (W * ss, (H + LBL) * ss), self.rgb(BG))
        d = ImageDraw.Draw(im)
        for y in (0, H / 2, H):  # gridlines at 100%, 50%, 0%
            d.line((0, y * ss, W * ss, y * ss), fill=self.rgb(TRACK), width=ss)
        slot = W / n
        bw = slot * 0.55
        try:
            lf = ImageFont.truetype("segoeui.ttf", self.px(9) * ss)
        except OSError:
            lf = ImageFont.load_default()
        for i in range(n):
            cx = (i + 0.5) * slot
            v = peak[i]
            if v > 0:
                top = H - max(self.px(3), H * min(v, 100) / 100)
                d.rounded_rectangle(((cx - bw / 2) * ss, top * ss, (cx + bw / 2) * ss, H * ss),
                                    radius=self.px(3) * ss, fill=self.rgb(color_for(v)))
            day = days[i]
            d.text((cx * ss, (H + self.px(3)) * ss), f"{day:%a}", anchor="mt", font=lf,
                   fill=self.rgb(FG if i == now_i else MUTED))
        if len(line) > 1:  # weekly % used over the week
            pts = [(x * W * ss, (H - H * min(y, 100) / 100) * ss) for x, y in line]
            d.line(pts, fill=self.rgb(FG2), width=max(1, self.px(1.5)) * ss, joint="curve")
        img = ImageTk.PhotoImage(im.resize((W, H + LBL), Image.LANCZOS))
        self._imgs["chart"] = img
        tk.Label(self.inner, image=img, bg=BG, bd=0).pack(anchor="w")
        lg = tk.Frame(self.inner, bg=BG)
        lg.pack(anchor="w", pady=(self.px(2), 0))
        tk.Label(lg, text="■", bg=BG, fg=CLAY, font=(FONT, 8)).pack(side="left")
        tk.Label(lg, text="Daily peak, 5-hour   ", bg=BG, fg=MUTED, font=(FONT, 8)).pack(side="left")
        tk.Label(lg, text="—", bg=BG, fg=FG2, font=(FONT, 8)).pack(side="left")
        tk.Label(lg, text="Weekly used", bg=BG, fg=MUTED, font=(FONT, 8)).pack(side="left")

    def render_main(self):
        self.clear_inner()
        if self.updated:
            mins = int((time.time() - self.updated) // 60)
            sub = "Updated just now" if mins < 1 else f"Updated {mins} min ago"
        else:
            sub = "Loading…"
        if self.plan:
            sub += f" · {self.plan}"
        left = self.manual_refresh_wait()
        self.header("Claude usage", sub, [("", self.manual_refresh, bool(left)),
                                          ("", lambda: self.render_settings())])
        if self.new_version:
            self.update_banner()
        if self.usage:
            for u in self.usage.values():
                self.bar(u["label"], u["pct"], u["resets_at"], u.get("note", ""))
            if self.settings["show_credits"]:
                self.credits_section()
            if self.settings["show_chart"]:
                self.history_chart()
        if self.error:
            tk.Label(self.inner, text=self.error, bg=BG, fg=AMBER, font=(FONT, 8), wraplength=self.px(280),
                     justify="left").pack(anchor="w", pady=(self.px(8), 0))
        elif self.source:
            tk.Label(self.inner, text=f"Source: {self.source}", bg=BG, fg=MUTED, font=(FONT, 8)).pack(anchor="w", pady=(self.px(8), 0))
        self.view = "main"
        self.position_popup()

    def update_banner(self):
        """Clickable 'new version available' strip that opens the GitHub release page."""
        box = tk.Frame(self.inner, bg=BG2, highlightthickness=1, highlightbackground=CLAY, cursor="hand2")
        box.pack(fill="x", pady=(0, self.px(12)))
        t = tk.Label(box, text=f"New version {self.new_version} available", bg=BG2, fg=FG,
                     font=(FONT, 9, "bold"), cursor="hand2")
        t.pack(side="left", padx=self.px(10), pady=self.px(6))
        a = tk.Label(box, text="Press here  ↗", bg=BG2, fg=CLAY, font=(FONT, 9), cursor="hand2")
        a.pack(side="right", padx=self.px(10))
        for w in (box, t, a):
            w.bind("<Button-1>", lambda e: webbrowser.open(RELEASES_URL))

    def check_for_update(self):
        if DEMO:
            return

        def work():
            try:
                latest = latest_release_version()
            except Exception as e:
                log(f"Update check failed: {type(e).__name__}")
                return
            if latest and version_tuple(latest) > version_tuple(__version__):
                self.q.put(lambda: self.on_new_version(latest))
        threading.Thread(target=work, daemon=True).start()

    def on_new_version(self, version):
        self.new_version = version
        if self.popup.winfo_viewable() and self.view == "main":
            self.render_main()

    # ---------------- settings page (same look as the main page)
    def _img(self, key, make):
        if key not in self._imgs:
            self._imgs[key] = ImageTk.PhotoImage(make())
        return self._imgs[key]

    def toggle_img(self, on):
        """Rounded on/off switch: Claude orange when on."""
        def make():
            ss, w, h = 4, self.px(34), self.px(18)
            m = self.px(3) * ss
            im = Image.new("RGB", (w * ss, h * ss), self.rgb(BG))
            d = ImageDraw.Draw(im)
            d.rounded_rectangle((0, 0, w * ss - 1, h * ss - 1), radius=h * ss / 2,
                                fill=self.rgb(CLAY if on else TRACK))
            k = h * ss - 2 * m
            x = (w * ss - k - m) if on else m
            d.ellipse((x, m, x + k, m + k), fill=(255, 255, 255) if on else self.rgb(FG2))
            return im.resize((w, h), Image.LANCZOS)
        return self._img(f"toggle_{on}", make)

    def chip_img(self, text, on):
        """Rounded pill used for choices (refresh interval, thresholds, source)."""
        def make():
            ss, h, pad = 4, self.px(22), self.px(10)
            try:
                f = ImageFont.truetype("segoeuib.ttf" if on else "segoeui.ttf", self.px(11) * ss)
            except OSError:
                f = ImageFont.load_default()
            tw = ImageDraw.Draw(Image.new("RGB", (1, 1))).textlength(text, font=f) / ss
            w = int(tw + 2 * pad)
            im = Image.new("RGB", (w * ss, h * ss), self.rgb(BG))
            d = ImageDraw.Draw(im)
            if on:
                d.rounded_rectangle((0, 0, w * ss - 1, h * ss - 1), radius=h * ss / 2, fill=self.rgb(CLAY))
            else:
                d.rounded_rectangle((0, 0, w * ss - 1, h * ss - 1), radius=h * ss / 2,
                                    fill=self.rgb(BG2), outline=self.rgb(TRACK), width=ss)
            d.text((w * ss / 2, h * ss / 2), text, anchor="mm", font=f,
                   fill=(44, 18, 8) if on else self.rgb(FG2))
            return im.resize((w, h), Image.LANCZOS)
        return self._img(f"chip_{text}_{on}", make)

    def render_settings(self):
        self.clear_inner()
        self.header("Settings", "Changes save automatically", [("", self.render_main)])
        s = self.settings
        W = self.px(280)

        def section(text):
            tk.Label(self.inner, text=text.upper(), bg=BG, fg=MUTED, font=(FONT, 7, "bold")).pack(
                anchor="w", pady=(self.px(8), self.px(2)))

        def row(title, desc=""):
            r = tk.Frame(self.inner, bg=BG)
            r.pack(fill="x", pady=self.px(4))
            left = tk.Frame(r, bg=BG)
            left.pack(side="left", fill="x", expand=True)
            tk.Label(left, text=title, bg=BG, fg=FG, font=(FONT, 9)).pack(anchor="w")
            if desc:
                tk.Label(left, text=desc, bg=BG, fg=MUTED, font=(FONT, 8), wraplength=self.px(210),
                         justify="left").pack(anchor="w")
            right = tk.Frame(r, bg=BG)
            right.pack(side="right")
            return right

        def labeled(title):
            r = tk.Frame(self.inner, bg=BG)
            r.pack(fill="x", pady=self.px(4))
            if title:
                tk.Label(r, text=title, bg=BG, fg=FG, font=(FONT, 9)).pack(anchor="w", pady=(0, self.px(4)))
            cf = tk.Frame(r, bg=BG)
            cf.pack(anchor="w")
            return cf

        def clickable(parent, image, fn, side="right", padx=0):
            lab = tk.Label(parent, image=image, bg=BG, bd=0, cursor="hand2")
            lab.pack(side=side, padx=padx)
            lab.bind("<Button-1>", lambda e: fn())
            return lab

        def toggle(key, title, desc=""):
            clickable(row(title, desc), self.toggle_img(s[key]), lambda: self.set_setting(key, not s[key]))

        def chips(key, options, parent):
            for val, text in options:
                clickable(parent, self.chip_img(text, s[key] == val), lambda v=val: self.set_setting(key, v),
                          side="left", padx=(0, self.px(4)))

        # One tab at a time keeps the page short.
        tabs = [("general", "General"), ("display", "Display"), ("alerts", "Alerts"), ("account", "Account")]
        tab = getattr(self, "settings_tab", "general")

        def multi(parent, items):
            for key, text in items:
                clickable(parent, self.chip_img(text, s[key]), lambda k=key: self.set_setting(k, not s[k]),
                          side="left", padx=(0, self.px(4)))

        if tab == "general":
            section("Startup")
            toggle("autostart", "Start with Windows")
            toggle("check_updates", "Check for updates", "Banner when a new version is out")
            chips("refresh_minutes", [(5, "5 min"), (10, "10 min"), (15, "15 min"), (30, "30 min")],
                  labeled("Refresh every"))

        elif tab == "display":
            section("Taskbar")
            toggle("show_taskbar_widget", "Usage next to the clock")
            if s["show_taskbar_widget"]:
                multi(labeled(""), (("widget_five", "5-hour"), ("widget_weekly", "Weekly"), ("widget_timer", "Time left")))
                multi(labeled(""), (("widget_square", "Square"), ("widget_outline", "Outline")))
            chips("tray_show", [("off", "Off"), ("five_hour", "5-hour"), ("seven_day", "Weekly")],
                  labeled("Number in the tray icon"))
            section("Popup")
            toggle("show_credits", "Credits & resets", "With the link to the usage page")
            toggle("show_chart", "This week's graph")

        elif tab == "alerts":
            section("Notifications")
            multi(labeled("When this much is left"), [(f"notify_{t}", f"{t}%") for t in (20, 10, 5)])
            toggle("notify_weekly", "Include weekly limits", "Weekly and per-model limits, like Fable")
            cf = labeled("When a limit resets")
            types = ([(k, u["label"]) for k, u in self.usage.items()] if self.usage
                     else [("five_hour", "5-hour session"), ("seven_day", "Weekly, all models")])
            for key, label in types:
                on = self.reset_enabled(key)
                clickable(cf, self.chip_img(short_name(key, label), on),
                          lambda k=key, o=on: self.set_reset_key(k, not o), side="left", padx=(0, self.px(4)))

        else:  # account
            section("Data source")
            chips("source", [("auto", "Auto"), ("claude_code", "Claude Code"), ("claude_ai", "claude.ai")], labeled(""))
            toggle("allow_token_refresh", "Renew Claude Code login", "When it has expired")
            section("claude.ai")
            r = row("Logged in" if s["session_key"] else "Not logged in",
                    "Resets and credits are shown" if s["session_key"]
                    else "Log in to see limit resets and credits")
            if getattr(self, "_login_proc", None):
                tk.Label(r, text="Logging in…", bg=BG, fg=MUTED, font=(FONT, 9)).pack()
            elif s["session_key"]:
                clickable(r, self.chip_img("Log out", False), self.logout_claude_ai)
            else:
                clickable(r, self.chip_img("Log in", True), self.start_login)
                tk.Label(self.inner, text="Or paste the sessionKey cookie", bg=BG, fg=FG2,
                         font=(FONT, 8)).pack(anchor="w", pady=(self.px(6), self.px(4)))
                box = tk.Frame(self.inner, bg=BG2, highlightthickness=1, highlightbackground=TRACK, highlightcolor=CLAY)
                box.pack(fill="x")
                e = tk.Entry(box, show="•", bg=BG2, fg=FG, insertbackground=FG, relief="flat", bd=0, font=(FONT, 9))
                e.pack(fill="x", padx=self.px(8), pady=self.px(5))
                e.bind("<KeyRelease>", lambda ev: self.set_session_key(e.get()))

        tk.Frame(self.inner, bg=TRACK, height=1, width=W).pack(fill="x", pady=(self.px(12), self.px(8)))
        foot = tk.Frame(self.inner, bg=BG)
        foot.pack(fill="x")
        q = tk.Label(foot, text="Quit Claude Usage", bg=BG, fg=MUTED, font=(FONT, 9), cursor="hand2")
        q.pack(side="left")
        tk.Label(foot, text=f"Version {__version__}", bg=BG, fg=MUTED, font=(FONT, 8)).pack(side="right")
        if tab == "general":
            tk.Label(self.inner, text="Unofficial app, not affiliated with or endorsed by Anthropic. "
                                      "Claude is a trademark of Anthropic, PBC.",
                     bg=BG, fg=MUTED, font=(FONT, 7), wraplength=W, justify="left").pack(anchor="w", pady=(self.px(8), 0))
        q.bind("<Button-1>", lambda ev: self.quit())
        q.bind("<Enter>", lambda ev: q.configure(fg=RED))
        q.bind("<Leave>", lambda ev: q.configure(fg=MUTED))
        # Tabs last: the popup is anchored at the bottom, so they stay put when switching tabs.
        tk.Frame(self.inner, bg=TRACK, height=1, width=W).pack(fill="x", pady=(self.px(10), self.px(8)))
        bar = tk.Frame(self.inner, bg=BG)
        bar.pack(fill="x")
        for key, text in tabs:
            clickable(bar, self.chip_img(text, tab == key), lambda k=key: self.show_settings_tab(k),
                      side="left", padx=(0, self.px(4)))
        self.view = "settings"
        self.position_popup()

    def show_settings_tab(self, tab):
        self.settings_tab = tab
        self.render_settings()

    def set_setting(self, key, value):
        s = self.settings
        old = s[key]
        s[key] = value
        save_json(SETTINGS_PATH, s)
        if key == "autostart" and value != old:
            try:
                set_autostart(value)
            except OSError:
                pass
        if key == "show_taskbar_widget":
            self.place_widget()
        if key.startswith("widget_") or key == "tray_show":
            self.update_widget()
        if key == "check_updates" and value and not old:
            self.check_for_update()
        if key in ("source", "allow_token_refresh"):
            self.next_fetch = time.time() + 1
        self.render_settings()

    def reset_enabled(self, key):
        return bool(self.settings["notify_reset_keys"].get(key, self.settings["notify_reset"]))

    def set_reset_key(self, key, value):
        keys = dict(self.settings["notify_reset_keys"])
        keys[key] = value
        self.set_setting("notify_reset_keys", keys)

    def start_login(self):
        """Open a claude.ai window (separate process); it hands back the session key after login."""
        try:
            os.remove(LOGIN_RESULT)
        except OSError:
            pass
        if FROZEN:
            cmd = [sys.executable, "--login"]
        else:
            exe = sys.executable
            if exe.lower().endswith("python.exe"):
                exe = exe[:-10] + "pythonw.exe"
            cmd = [exe, os.path.abspath(__file__), "--login"]
        import subprocess
        try:
            self._login_proc = subprocess.Popen(cmd)
        except OSError as e:
            log(f"Couldn't open the login window: {e}")
            return
        self.hide_popup()
        self.root.after(1000, self._check_login)

    def _check_login(self):
        if self._login_proc.poll() is None:
            self.root.after(1000, self._check_login)
            return
        self._login_proc = None
        key = (load_json(LOGIN_RESULT, {}) or {}).get("key", "")
        try:
            os.remove(LOGIN_RESULT)
        except OSError:
            pass
        if key:
            self.settings["session_key"] = key
            save_json(SETTINGS_PATH, self.settings)
            _ai_extras["t"] = 0  # ask claude.ai on the next refresh (at most 5 minutes away)
            self.next_fetch = time.time() + 1
            self.notify("Logged in to claude.ai", "Limit resets and credits will show in Claude Usage.")
            log("Logged in to claude.ai")
        if self.popup.winfo_viewable() and self.view == "settings":
            self.render_settings()

    def logout_claude_ai(self):
        self.settings["session_key"] = ""
        save_json(SETTINGS_PATH, self.settings)
        _ai_extras.update(t=0, data=None)
        save_fetch_state(ai_t=0, ai_data=None)
        if self.credits:
            self.credits = dict(self.credits, extras=[x for x in self.credits.get("extras") or []
                                                      if x["key"].split(":")[0] not in RESET_FIELDS])
        self.render_settings()

    def set_session_key(self, value):
        self.settings["session_key"] = value.strip()
        save_json(SETTINGS_PATH, self.settings)
        self.next_fetch = time.time() + 2  # wait until typing stops

    def position_popup(self):
        p = self.popup
        p.update_idletasks()
        pw, ph = p.winfo_reqwidth(), p.winfo_reqheight()
        l, t, r, b = work_area()
        if self.settings["show_taskbar_widget"] and getattr(self, "w_hwnd", None):
            wr = self.widget_rect()
            x = wr.right - pw + self.px(40)
        else:
            x = r - pw - self.px(12)
        x = max(l + self.px(8), min(x, r - pw - self.px(8)))
        y = b - ph - self.px(12)
        p.geometry(f"+{x}+{y}")
        old = getattr(self, "_old_popup", None)
        if old is None:
            return
        self._old_popup = None
        old_imgs = self._old_imgs  # released when this function returns, after old is destroyed
        self._old_imgs = None
        if old.winfo_viewable():
            # Let the new window draw itself while still invisible, then show it and drop the old one.
            p.update_idletasks()
            p.update()
            p.attributes("-alpha", 1.0)
            p.lift()
            old.destroy()
            p.focus_force()
            user32.SetForegroundWindow(hwnd_of(p))
            self.popup_opened_at = time.time()
            self.popup_had_focus = False
        else:
            p.withdraw()
            p.attributes("-alpha", 1.0)
            old.destroy()

    def show_popup(self, settings=False):
        self.render_settings() if settings else self.render_main()
        self.popup.deiconify()
        self.popup.lift()
        self.popup.focus_force()
        user32.SetForegroundWindow(hwnd_of(self.popup))
        self.popup_opened_at = time.time()
        self.popup_had_focus = False
        self.mouse_hook(True)

    def hide_popup(self):
        self.popup.withdraw()
        self.mouse_hook(False)

    def mouse_hook(self, on):
        """Watch mouse clicks system-wide while the popup is open (removed when it closes)."""
        hook = getattr(self, "_hook", None)
        if on and not hook:
            self._clicks = []

            def proc(n, wp, lp):
                try:
                    if n == 0 and wp in WM_BUTTONDOWNS:
                        info = ctypes.cast(lp, ctypes.POINTER(MSLLHOOKSTRUCT)).contents
                        self._clicks.append((info.pt.x, info.pt.y))
                except Exception:
                    pass
                return user32.CallNextHookEx(None, n, wp, lp)
            self._hook_proc = HOOKPROC(proc)  # keep a reference, or it's garbage collected
            self._hook = user32.SetWindowsHookExW(
                14, self._hook_proc, ctypes.windll.kernel32.GetModuleHandleW(None), 0)  # WH_MOUSE_LL
        elif not on and hook:
            user32.UnhookWindowsHookEx(hook)
            self._hook = None

    def toggle_popup(self):
        if self.popup.winfo_viewable():
            self.hide_popup()
        else:
            self.show_popup()

    # ---------------- tray icon
    def tray_image(self):
        size = 64
        img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
        d = ImageDraw.Draw(img)
        key = self.settings["tray_show"]
        if self.usage and key in self.usage:
            pct = self.usage[key]["pct"]
            col = color_for(pct)
            d.rounded_rectangle((0, 0, size - 1, size - 1), 12, fill=col)
            text = f"{min(pct, 99):.0f}"
            try:
                font = ImageFont.truetype("segoeuib.ttf", 40 if len(text) < 2 else 36)
            except OSError:
                font = ImageFont.load_default()
            d.text((size / 2, size / 2), text, fill="#1b1b1b", font=font, anchor="mm")
        else:
            img = draw_logo(size)
        return img

    def build_tray(self):
        def post(fn):
            return lambda icon, item: self.q.put(fn)
        menu = pystray.Menu(
            pystray.MenuItem("Open", post(self.toggle_popup), default=True),
            pystray.MenuItem("Refresh now", post(self.manual_refresh)),
            pystray.MenuItem("Settings", post(lambda: self.show_popup(settings=True))),
            pystray.MenuItem("Quit", post(self.quit)),
        )
        self.tray = pystray.Icon(APP_NAME, self.tray_image(), "Claude usage", menu)
        self.tray.run_detached()

    def update_tray(self):
        """Tray icon number and tooltip, following the "Number in the tray icon" setting."""
        tray = getattr(self, "tray", None)
        if not tray:
            return
        img = self.tray_image()
        key = self.settings["tray_show"]
        if self.usage:
            lines = [f"{u['label']}: {u['pct']:.0f}% used" for k, u in self.usage.items()
                     if k in ("five_hour", "seven_day")]
            title = "Claude usage\n" + "\n".join(lines)
            if key in self.usage:
                title += "\n" + fmt_reset(self.usage[key]["resets_at"])
        else:
            title = "Claude usage"
        sig = (img.tobytes(), title)
        if sig != getattr(self, "_tray_sig", None):  # only touch the icon when it changed
            self._tray_sig = sig
            tray.icon = img
            tray.title = title[:127]  # Windows tooltip limit

    def notify(self, title, msg):
        try:
            self.tray.notify(msg, title)
        except Exception:
            pass

    # ---------------- loop
    def pump(self):
        try:
            while True:
                item = self.q.get_nowait()
                if callable(item):
                    item()
                else:
                    self.on_result(*item)
        except queue.Empty:
            pass
        self.root.after(200, self.pump)

    def keep_on_top(self):
        """Put the widget back above the taskbar if something (e.g. the snipping tool) pushed it under."""
        if self.settings["show_taskbar_widget"] and getattr(self, "w_hwnd", None):
            user32.SetWindowPos(self.w_hwnd, wt.HWND(HWND_TOPMOST), 0, 0, 0, 0, SWP_FLAGS)
        self.root.after(300, self.keep_on_top)

    def tick(self):
        self.place_widget()
        if self.popup.winfo_viewable() and time.time() - self.popup_opened_at > 0.6:
            fg = user32.GetForegroundWindow()
            ours = fg == hwnd_of(self.popup) or self._menu_open()
            if ours:
                self.popup_had_focus = True
            elif self.popup_had_focus:  # user clicked somewhere else
                self.hide_popup()
        if time.time() >= self.next_fetch:
            self.request_fetch()
        minute = int(time.time() // 60)
        if minute != getattr(self, "_last_minute", None):  # keep the countdown current
            self._last_minute = minute
            self.update_widget()
        self.root.after(1000, self.tick)

    def watch_outside_click(self):
        """Close the popup when the mouse is pressed anywhere outside it.

        The focus check in tick() alone wasn't enough: Windows often won't give the popup
        focus, so it never "lost" it and stayed open.
        """
        try:
            # 0x8000 = held now, 0x0001 = pressed since the last check (catches quick clicks).
            # Read every time, even while closed, so a click from before the popup opened is
            # cleared instead of closing it right away.
            pressed = [bool(user32.GetAsyncKeyState(vk) & 0x8001) for vk in (0x01, 0x02, 0x04)]
            clicks, self._clicks = getattr(self, "_clicks", []), []  # from the mouse hook
            # While a page is being swapped (gear / back), self.popup is the new, differently sized
            # window, so a click on the old one would look "outside" and close the popup.
            swapping = getattr(self, "_old_popup", None) is not None
            if not swapping and self.popup.winfo_viewable() and time.time() - self.popup_opened_at > 0.3:
                points = [wt.POINT(x, y) for x, y in clicks]
                if any(pressed):  # fallback if the hook isn't available
                    pt = wt.POINT()
                    user32.GetCursorPos(ctypes.byref(pt))
                    points.append(pt)
                w = getattr(self, "w_hwnd", None)
                for pt in points:
                    inside = self._point_in(hwnd_of(self.popup), pt)
                    on_widget = bool(w) and self.settings["show_taskbar_widget"] and self._point_in(w, pt)
                    if not inside and not on_widget and not self._menu_open():
                        self.hide_popup()
                        break
        finally:
            self.root.after(50, self.watch_outside_click)

    @staticmethod
    def _point_in(hwnd, pt):
        rc = wt.RECT()
        user32.GetWindowRect(hwnd, ctypes.byref(rc))
        return rc.left <= pt.x < rc.right and rc.top <= pt.y < rc.bottom

    def _menu_open(self):
        buf = ctypes.create_unicode_buffer(64)
        user32.GetClassNameW(user32.GetForegroundWindow(), buf, 64)
        return buf.value in ("#32768", "TkChild")

    def manual_refresh_wait(self):
        """Seconds left before the refresh button can be used again (0 = available)."""
        left = max(self.last_fetch + MIN_FETCH_GAP - time.time(), self.rate_until - time.time())
        return int(left) + 1 if left > 0 else 0

    def manual_refresh(self):
        if self.manual_refresh_wait():
            return
        self.request_fetch()
        self.root.after(MIN_FETCH_GAP * 1000 + 200, self._refresh_unlocked)

    def _refresh_unlocked(self):
        if self.popup.winfo_viewable() and self.view == "main":
            self.render_main()

    def request_fetch(self):
        now = time.time()
        if now < self.rate_until:
            return  # the server asked us to wait; next_fetch already points past that
        if self.fetching:
            return
        earliest = self.last_fetch + MIN_FETCH_GAP
        if now < earliest:  # asked too soon (reset time, settings change...): do it when allowed
            self.next_fetch = earliest
            return
        self.last_fetch = now
        save_fetch_state(last_fetch=now)
        self.next_fetch = now + self.settings["refresh_minutes"] * 60
        self.fetching = True

        def work():
            try:
                usage, plan, credits, src = fetch(self.settings)
                self.q.put(("ok", usage, plan, src, credits))
            except RateLimited as e:
                self.q.put(("rate", e.retry_after, None, None, None))
            except Exception as e:
                self.q.put(("err", str(e), None, None, None))
        threading.Thread(target=work, daemon=True).start()

    def on_result(self, kind, a, b, c, d=None):
        self.fetching = False
        if kind == "rate":
            # Back off: the server's wait time, doubled for each rate limit in a row (max 10 min).
            self._rate_strikes = getattr(self, "_rate_strikes", 0) + 1
            wait = min(600, max(a, 60) * 2 ** (self._rate_strikes - 1))
            self.rate_until = self.next_fetch = time.time() + wait
            save_fetch_state(rate_until=self.rate_until)
            when = datetime.fromtimestamp(self.next_fetch)
            shown = "Showing the last numbers; t" if self.usage else "T"
            self.error = f"Too many usage requests right now. {shown}rying again at {when:%H:%M}."
        elif kind == "err":
            self.error = a
        else:
            self.error = None
            self._rate_strikes = 0
            self.usage, self.plan, self.source, self.credits = a, b, c, d
            self.updated = time.time()
            if not DEMO:
                save_last_usage(self.usage, self.plan, self.source, self.credits)
            self.record_history()
            self.check_notifications()
            for u in self.usage.values():  # fetch shortly after any reset so the number drops right away
                if u["resets_at"]:
                    secs = (u["resets_at"] - datetime.now(timezone.utc)).total_seconds() + 20
                    if 0 < secs and time.time() + secs < self.next_fetch:
                        self.next_fetch = time.time() + secs
        self.update_widget()  # also updates the tray icon
        if self.popup.winfo_viewable() and self.view == "main":
            self.render_main()

    def record_history(self):
        sd = self.usage.get("seven_day", {}).get("pct")
        self.history.append({"t": int(time.time()), "fh": self.usage["five_hour"]["pct"], "sd": sd})
        cutoff = time.time() - 31 * 86400
        self.history = [h for h in self.history if h["t"] >= cutoff]
        if not DEMO:
            save_json(HISTORY_PATH, self.history)

    def check_notifications(self):
        s = self.settings
        """Low-usage alerts fire once per threshold (20, 10, 5% left) and are only re-armed after a
        reset. resets_at jitters by a second between fetches, so it can't be used as a period id."""
        s = self.settings
        thresholds = [t for t in (20, 10, 5) if s[f"notify_{t}"]]
        now = datetime.now(timezone.utc)
        for key, u in self.usage.items():
            pct, reset = u["pct"], u["resets_at"]
            st = self.notified.get(key) or {}
            done = set(st.get("done", []))
            prev_pct = st.get("pct")
            prev_reset = parse_time(st.get("reset"))

            # Did this limit reset since the last reading? Usage dropped, and either the old
            # reset time has passed or the new one is clearly later.
            was_reset = (prev_pct is not None and pct < prev_pct - 1 and (
                prev_reset is None or reset is None or now >= prev_reset - timedelta(minutes=1)
                or reset > prev_reset + timedelta(hours=1)))
            if was_reset:
                done = set()
                if self.reset_enabled(key) and prev_pct >= 10:
                    name = limit_name(key, u["label"])
                    self.notify(f"Your {name} has reset", f"You're at {pct:.0f}% used. Time to continue!")
            # Re-arm a threshold if usage is clearly back above it (covers resets we missed).
            done = {t for t in done if 100 - pct <= t + 5}

            if not (u["weekly"] and not s["notify_weekly"]):
                left = 100 - pct
                crossed = [t for t in thresholds if left <= t and t not in done]
                if crossed:
                    name = limit_name(key, u["label"])
                    self.notify(f"{left:.0f}% of your {name} left", fmt_reset(reset))
                    done.update(crossed)
            self.notified[key] = {"done": sorted(done), "pct": pct,
                                  "reset": reset.isoformat() if reset else None}
        # Something new you can use (a reset, offer, promo...): tell once per item.
        extras = (self.credits or {}).get("extras") or []
        seen = set(self.notified.get("_extras", []))
        for x in extras:
            if x["key"] not in seen:
                exp = parse_time(x.get("expires"))
                when = f" Expires {exp.astimezone():%a %d %b %H:%M}." if exp else ""
                self.notify(f"You have something to use: {x['name']}", f"Open Claude Usage for details.{when}")
        self.notified["_extras"] = [x["key"] for x in extras]  # forget ones that are gone
        if not DEMO:
            save_json(os.path.join(DATA_DIR, "notified.json"), self.notified)

    def quit(self):
        log("Quit chosen by user")
        self.mouse_hook(False)
        try:
            self.tray.stop()
        except Exception:
            pass
        self.root.destroy()

    def run(self):
        self.root.mainloop()


LOG_PATH = os.path.join(DATA_DIR, "app.log")


def log(msg):
    """Append a line to %APPDATA%\\ClaudeUsage\\app.log (kept small)."""
    try:
        os.makedirs(DATA_DIR, exist_ok=True)
        if os.path.exists(LOG_PATH) and os.path.getsize(LOG_PATH) > 512 * 1024:
            os.replace(LOG_PATH, LOG_PATH + ".old")
        with open(LOG_PATH, "a", encoding="utf-8") as f:
            f.write(f"{datetime.now():%Y-%m-%d %H:%M:%S} v{__version__} {msg}\n")
    except Exception:
        pass


def log_exception(prefix, exc_type, exc, tb):
    import traceback
    log(prefix + "\n" + "".join(traceback.format_exception(exc_type, exc, tb)))


LOGIN_RESULT = os.path.join(DATA_DIR, "login_result.json")
LOGIN_TITLE = "Log in to claude.ai"


def app_icon_path():
    """The app icon (orange square, two dark blocks) as an .ico, made once in DATA_DIR."""
    path = os.path.join(DATA_DIR, "app.ico")
    if not os.path.exists(path):
        os.makedirs(DATA_DIR, exist_ok=True)
        draw_logo(256).save(path, sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (256, 256)])
    return path


def set_window_icon(title):
    """Replace the default (Python) icon of a window, title bar and taskbar."""
    hwnd = user32.FindWindowW(None, title)
    if not hwnd:
        return False
    user32.LoadImageW.restype = ctypes.c_void_p
    user32.SendMessageW.argtypes = [wt.HWND, ctypes.c_uint, wt.WPARAM, ctypes.c_void_p]
    path = app_icon_path()
    for which, size in ((0, 16), (1, 32)):  # ICON_SMALL, ICON_BIG
        h = user32.LoadImageW(None, path, 1, int(size * dpi_scale()), int(size * dpi_scale()), 0x10)
        if h:
            user32.SendMessageW(hwnd, 0x0080, which, h)  # WM_SETICON
    return True


def run_login():
    """Login window: claude.ai in an embedded browser. Once signed in, the sessionKey cookie
    is written to LOGIN_RESULT for the main app and the window closes."""
    import webview

    def watch(win):
        icon_set = False
        for _ in range(20 * 60):  # give up after 20 minutes
            time.sleep(1)
            if not icon_set:
                try:
                    icon_set = set_window_icon(LOGIN_TITLE)
                except Exception:
                    icon_set = True
            try:
                for c in win.get_cookies():
                    m = c.get("sessionKey")
                    if m is not None and m.value.startswith("sk-ant-"):
                        save_json(LOGIN_RESULT, {"key": m.value})
                        win.destroy()
                        return
            except Exception:
                pass  # window still loading, or already closed
        win.destroy()

    win = webview.create_window(LOGIN_TITLE, "https://claude.ai/login", width=480, height=760)
    webview.start(watch, win, private_mode=True)  # nothing kept on disk except the key we save


if __name__ == "__main__" and "--login" in sys.argv:
    try:
        run_login()
    except Exception:
        log_exception("Login window failed:", *sys.exc_info())
    sys.exit(0)

if __name__ == "__main__":
    sys.excepthook = lambda t, e, tb: log_exception("Crash:", t, e, tb)
    threading.excepthook = lambda a: log_exception(f"Thread error ({a.thread.name}):", a.exc_type, a.exc_value, a.exc_traceback)
    # Only one instance at a time.
    mutex = ctypes.windll.kernel32.CreateMutexW(None, False, "Local\\ClaudeUsageTrayApp")
    if ctypes.windll.kernel32.GetLastError() == 183:  # ERROR_ALREADY_EXISTS
        # Say so instead of quietly doing nothing; only one copy can run at a time.
        ctypes.windll.user32.MessageBoxW(
            None, "Claude Usage is already running.\n\nLook for it next to the clock or in the tray. "
                  "To start this copy instead, quit the running one first (right-click it → Quit).",
            "Claude Usage", 0x40 | 0x40000)  # MB_ICONINFORMATION | MB_TOPMOST
        sys.exit(0)
    log("Started")
    try:
        App().run()
        log("Stopped")
    except BaseException:
        log_exception("Crash:", *sys.exc_info())
        raise
