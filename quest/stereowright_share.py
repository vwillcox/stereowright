#!/usr/bin/env python3
"""Stereowright Quest share: view your Stereowright 3D photos on a Meta Quest.

A small, separate helper. Stereowright itself never uses the network; this program is
the one thing you start on purpose to get photos into your headset. It:

  * serves ONLY Stereowright's 3D outputs (*_LR / *_3dh / *_TB .jpg/.png) from the
    output folders Stereowright recorded (plus any --folder you pass), read-only;
  * never makes an outgoing connection (no internet, no telemetry, no CDNs:
    the 3D viewer it serves is a few local files);
  * USB: listens on 127.0.0.1 only. The headset reaches it through the cable
    (`adb reverse`), so the photos never touch any network;
  * Wi-Fi: listens on your LAN address only, refuses non-private client
    addresses, needs a secret token in the URL, and uses HTTPS with a
    certificate made on this computer.

    python3 -I quest/stereowright_share.py               # USB + Wi-Fi
    python3 -I quest/stereowright_share.py --mode usb    # cable only, no network at all
    python3 -I quest/stereowright_share.py --help

Standard library only. Uses `adb` (android-tools) for USB, `openssl` to make the
Wi-Fi certificate once, and `magick` (optional) for gallery thumbnails.
"""

from __future__ import annotations

import argparse
import hashlib
import hmac
import http.server
import ipaddress
import json
import mimetypes
import os
import re
import secrets
import shutil
import signal
import socketserver
import ssl
import subprocess
import sys
import threading
import time
from pathlib import Path
from urllib.parse import unquote, urlsplit

VERSION = "1"
HERE = Path(__file__).resolve().parent
VIEWER_DIR = HERE / "viewer"
STATIC = {
    "": ("index.html", "text/html; charset=utf-8"),
    "index.html": ("index.html", "text/html; charset=utf-8"),
    "viewer.js": ("viewer.js", "text/javascript; charset=utf-8"),
    "viewer.css": ("viewer.css", "text/css; charset=utf-8"),
    "icon.svg": ("icon.svg", "image/svg+xml"),
}
# Stereowright output names: <stem>_LR (full side-by-side), _3dh (half SBS), _TB (top/bottom).
PHOTO_RE = re.compile(r"^(?P<stem>.+)_(?P<kind>LR|3dh|TB)\.(?:jpe?g|png)$", re.IGNORECASE)
LAYOUT = {"lr": "sbs", "3dh": "hsbs", "tb": "tb"}
MAX_PHOTOS = 5000
QUEST_USB_VENDOR = "2833"  # Oculus VR / Meta


def log(msg: str) -> None:
    print(time.strftime("%H:%M:%S"), msg, flush=True)


def data_dir() -> Path:
    base = os.environ.get("XDG_DATA_HOME") or os.path.join(os.path.expanduser("~"), ".local/share")
    return Path(base) / "stereowright"


# --------------------------------------------------------------------------- state files


class State:
    """Files in ~/.local/share/stereowright/share/ (token, certificate, folders, status)."""

    def __init__(self, root: Path):
        self.root = root
        root.mkdir(parents=True, exist_ok=True)
        os.chmod(root, 0o700)
        self.thumbs = root / "thumbs"
        self.thumbs.mkdir(exist_ok=True)
        self.status_path = root / "status.json"
        self._status: dict = {}
        self._lock = threading.Lock()

    def token(self) -> str:
        p = self.root / "token"
        try:
            t = p.read_text().strip()
            if re.fullmatch(r"[a-z0-9]{10,64}", t):
                return t
        except FileNotFoundError:
            pass
        # Easy to type on a headset keyboard: lowercase letters and digits (~52 bits).
        alphabet = "abcdefghjkmnpqrstuvwxyz23456789"
        t = "".join(secrets.choice(alphabet) for _ in range(11))
        fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as f:
            f.write(t + "\n")
        return t

    def folders(self, extra: list[Path]) -> list[Path]:
        out: list[Path] = []
        try:
            lines = (self.root / "folders.txt").read_text().splitlines()
        except FileNotFoundError:
            lines = []
        for line in lines + [str(p) for p in extra]:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            p = Path(line).expanduser()
            if p.is_dir() and p not in out:
                out.append(p)
        return out

    def update_status(self, **kv) -> None:
        with self._lock:
            self._status.update(kv)
            self._status["updated"] = int(time.time())
            tmp = self.status_path.with_suffix(".tmp")
            tmp.write_text(json.dumps(self._status, indent=1))
            tmp.replace(self.status_path)

    def clear_status(self) -> None:
        try:
            self.status_path.unlink()
        except FileNotFoundError:
            pass


# --------------------------------------------------------------------------- photo index


class Library:
    def __init__(self, state: State, extra: list[Path]):
        self.state = state
        self.extra = extra
        self.by_id: dict[str, Path] = {}
        self._lock = threading.Lock()

    def scan(self) -> list[dict]:
        photos = []
        ids: dict[str, Path] = {}
        for folder in self.state.folders(self.extra):
            try:
                entries = list(os.scandir(folder))
            except OSError:
                continue
            for e in entries:
                m = PHOTO_RE.match(e.name)
                if not m or not e.is_file(follow_symlinks=False):
                    continue
                try:
                    st = e.stat(follow_symlinks=False)
                except OSError:
                    continue
                path = Path(e.path)
                pid = hashlib.sha256(f"{path}\0{st.st_mtime_ns}\0{st.st_size}".encode()).hexdigest()[:20]
                ids[pid] = path
                photos.append(
                    {
                        "id": pid,
                        "name": m.group("stem"),
                        "file": e.name,
                        "folder": folder.name if folder.name != "stereowright" else folder.parent.name,
                        "layout": LAYOUT[m.group("kind").lower()],
                        "mtime": int(st.st_mtime * 1000),
                        "size": st.st_size,
                    }
                )
        photos.sort(key=lambda p: p["mtime"], reverse=True)
        with self._lock:
            self.by_id = ids
        return photos[:MAX_PHOTOS]

    def path(self, pid: str) -> Path | None:
        with self._lock:
            p = self.by_id.get(pid)
        if p is None:
            self.scan()
            with self._lock:
                p = self.by_id.get(pid)
        return p


_thumb_slots = threading.Semaphore(2)


def thumbnail(state: State, pid: str, src: Path) -> Path | None:
    """Left-eye thumbnail via ImageMagick (cached). None if magick is unavailable."""
    out = state.thumbs / f"{pid}.jpg"
    if out.exists():
        return out
    magick = shutil.which("magick")
    if not magick:
        return None
    kind = PHOTO_RE.match(src.name).group("kind").lower()
    crop = {"lr": ["-crop", "50%x100%+0+0"], "3dh": ["-crop", "50%x100%+0+0", "-resize", "200%x100%!"],
            "tb": ["-crop", "100%x50%+0+0"]}[kind]
    tmp = out.with_suffix(".part.jpg")
    with _thumb_slots:
        if out.exists():
            return out
        try:
            subprocess.run(
                [magick, "-define", "jpeg:size=1200x800", str(src), *crop, "+repage",
                 "-thumbnail", "480x480>", "-quality", "82", "-strip", str(tmp)],
                check=True, timeout=60, stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
            tmp.replace(out)
        except (subprocess.SubprocessError, OSError):
            tmp.unlink(missing_ok=True)
            return None
    return out


# --------------------------------------------------------------------------- HTTP


class Handler(http.server.BaseHTTPRequestHandler):
    server_version = "StereowrightShare/" + VERSION
    sys_version = ""
    protocol_version = "HTTP/1.1"

    # Set per server:
    share: "Share"
    kind: str  # "usb" | "wifi"

    def log_message(self, fmt, *args):  # quiet by default
        if self.share.verbose:
            log(f"{self.kind} {self.client_address[0]} " + fmt % args)

    def deny(self, code=404):
        body = b"Not found\n" if code == 404 else b"Forbidden\n"
        self.send_response(code)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def allowed_client(self) -> bool:
        ip = ipaddress.ip_address(self.client_address[0])
        if self.kind == "usb":
            host = (self.headers.get("Host") or "").lower()
            port = self.server.server_address[1]
            # Host check stops DNS-rebinding tricks from web pages on this computer.
            return ip.is_loopback and host in (f"localhost:{port}", f"127.0.0.1:{port}")
        return ip.is_private or ip.is_link_local

    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        if not self.allowed_client():
            return self.deny(403)
        path = unquote(urlsplit(self.path).path)
        prefix = "/t/"
        if not path.startswith(prefix):
            return self.deny()
        token, _, rest = path[len(prefix):].partition("/")
        if not hmac.compare_digest(token.encode(), self.share.token.encode()):
            return self.deny()
        if not _ and not rest:  # "/t/<token>" without the slash
            self.send_response(301)
            self.send_header("Location", f"/t/{token}/")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        self.share.touch()
        if rest in STATIC:
            name, ctype = STATIC[rest]
            return self.send_file(VIEWER_DIR / name, ctype, cache="no-cache")
        if rest == "api/photos":
            return self.send_json(self.share.listing(self.kind))
        m = re.fullmatch(r"(photo|thumb)/([0-9a-f]{20})", rest)
        if m:
            src = self.share.library.path(m.group(2))
            if src is None:
                return self.deny()
            if m.group(1) == "thumb":
                t = thumbnail(self.share.state, m.group(2), src)
                if t is None:
                    return self.deny()
                return self.send_file(t, "image/jpeg", cache="private, max-age=31536000, immutable")
            ctype = mimetypes.guess_type(src.name)[0] or "application/octet-stream"
            return self.send_file(src, ctype, cache="private, max-age=31536000, immutable")
        return self.deny()

    def common_headers(self):
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header(
            "Content-Security-Policy",
            "default-src 'self'; img-src 'self' blob:; connect-src 'self'; "
            "style-src 'self'; script-src 'self'; frame-ancestors 'none'",
        )

    def send_json(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.common_headers()
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def send_file(self, path: Path, ctype: str, cache: str):
        try:
            f = open(path, "rb")
        except OSError:
            return self.deny()
        with f:
            size = os.fstat(f.fileno()).st_size
            self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(size))
            self.send_header("Cache-Control", cache)
            self.common_headers()
            self.end_headers()
            if self.command != "HEAD":
                try:
                    shutil.copyfileobj(f, self.wfile, 256 * 1024)
                except (BrokenPipeError, ConnectionResetError, ssl.SSLError, TimeoutError):
                    pass


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True
    tls: ssl.SSLContext | None = None

    def finish_request(self, request, client_address):
        # Runs in the per-connection thread, so a slow TLS handshake can't block others.
        request.settimeout(30)
        if self.tls is not None:
            try:
                request = self.tls.wrap_socket(request, server_side=True)
            except (ssl.SSLError, OSError):
                return  # e.g. the headset rejecting the self-made certificate before you accept it
        super().finish_request(request, client_address)

    def handle_error(self, request, client_address):
        pass  # dropped connections are normal (headset sleeping, page closed)


# --------------------------------------------------------------------------- network helpers


def lan_address() -> tuple[str, str] | None:
    """(ip, cidr) of the interface with the default route, read from `ip` (no packets sent)."""
    try:
        routes = json.loads(subprocess.run(["ip", "-j", "-4", "route", "show", "default"],
                                           capture_output=True, text=True, timeout=5).stdout or "[]")
        addrs = json.loads(subprocess.run(["ip", "-j", "-4", "addr", "show", "scope", "global"],
                                          capture_output=True, text=True, timeout=5).stdout or "[]")
    except (OSError, ValueError, subprocess.SubprocessError):
        return None
    devs = [r.get("dev") for r in routes if r.get("dev")]
    skip = ("docker", "br-", "veth", "virbr", "tun", "tap", "wg", "tailscale", "zt")
    cands = []
    for a in addrs:
        name = a.get("ifname", "")
        if name.startswith(skip):
            continue
        for info in a.get("addr_info", []):
            ip = info.get("local")
            if ip and ipaddress.ip_address(ip).is_private:
                net = ipaddress.ip_interface(f"{ip}/{info.get('prefixlen', 24)}").network
                cands.append((name not in devs, ip, str(net)))
    cands.sort()
    return (cands[0][1], cands[0][2]) if cands else None


def ensure_cert(state: State, ip: str) -> tuple[Path, Path]:
    cert, key = state.root / "cert.pem", state.root / "key.pem"
    ip_file = state.root / "cert-ip"
    if cert.exists() and key.exists() and ip_file.exists() and ip_file.read_text().strip() == ip:
        return cert, key
    openssl = shutil.which("openssl")
    if not openssl:
        raise SystemExit("openssl is needed once to make the Wi-Fi certificate (pacman -S openssl)")
    log("Making a certificate for Wi-Fi (HTTPS) on this computer…")
    old = os.umask(0o077)
    try:
        subprocess.run(
            [openssl, "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
             "-nodes", "-days", "3650", "-subj", "/CN=Stereowright Quest share",
             "-addext", f"subjectAltName=IP:{ip},DNS:localhost",
             "-keyout", str(key), "-out", str(cert)],
            check=True, capture_output=True, timeout=60,
        )
    finally:
        os.umask(old)
    ip_file.write_text(ip + "\n")
    return cert, key


def firewall_hint(port: int, cidr: str) -> str | None:
    """If ufw is active and has no rule for the port, the command that opens it to the LAN."""
    try:
        active = subprocess.run(["systemctl", "is-active", "ufw"], capture_output=True,
                                text=True, timeout=5).stdout.strip() == "active"
    except (OSError, subprocess.SubprocessError):
        return None
    if not active:
        return None
    try:
        rules = Path("/etc/ufw/user.rules").read_text()
    except OSError:
        rules = ""
    if re.search(rf"^### tuple ### allow (tcp|any) {port} ", rules, re.M):
        return None
    return f"sudo ufw allow from {cidr} to any port {port} proto tcp comment 'Stereowright Quest share'"


def quest_on_usb() -> bool:
    """A Meta headset is plugged in (seen by vendor id; works without USB debugging)."""
    for d in Path("/sys/bus/usb/devices").glob("*/idVendor"):
        try:
            if d.read_text().strip() == QUEST_USB_VENDOR:
                return True
        except OSError:
            pass
    return False


# --------------------------------------------------------------------------- USB / adb


class UsbLink(threading.Thread):
    """Watches for the Quest on USB; tunnels the viewer through the cable and opens it."""

    def __init__(self, share: "Share", adb: str | None, port: int, open_browser: bool):
        super().__init__(name="usb", daemon=True)
        self.share, self.adb, self.port, self.open_browser = share, adb, port, open_browser
        self.ready: set[str] = set()
        self.last_msg = ""

    def run(self):
        while not self.share.stopping.is_set():
            try:
                self.tick()
            except Exception as e:  # keep watching whatever happens
                self.say("error", f"USB: {e}")
            self.share.stopping.wait(2.5)

    def say(self, state: str, msg: str):
        self.share.state.update_status(usb={"state": state, "message": msg,
                                            "url": self.share.url("usb")})
        if msg != self.last_msg:
            log(msg)
            self.last_msg = msg

    def adb_run(self, *args, timeout=10) -> subprocess.CompletedProcess:
        return subprocess.run([self.adb, *args], capture_output=True, text=True,
                              timeout=timeout, stdin=subprocess.DEVNULL)

    def tick(self):
        if not self.adb:
            self.say("no-adb", "USB: adb is not installed (pacman -S android-tools).")
            return
        out = self.adb_run("devices", "-l").stdout
        devices = {}
        for line in out.splitlines()[1:]:
            parts = line.split()
            if len(parts) >= 2:
                devices[parts[0]] = (parts[1], " ".join(parts[2:]))
        self.ready &= {s for s, (st, _) in devices.items() if st == "device"}
        good = [s for s, (st, _) in devices.items() if st == "device"]
        if good:
            for serial in good:
                if serial in self.ready:
                    continue
                r = self.adb_run("-s", serial, "reverse", f"tcp:{self.port}", f"tcp:{self.port}")
                if r.returncode != 0:
                    self.say("error", f"USB: could not connect the cable tunnel: {r.stderr.strip()}")
                    continue
                self.ready.add(serial)
                if self.open_browser:
                    url = self.share.url("usb")
                    r = self.adb_run("-s", serial, "shell", "am", "start", "-a",
                                     "android.intent.action.VIEW", "-d", url, "com.oculus.browser")
                    if r.returncode != 0 or "Error" in r.stdout:
                        self.adb_run("-s", serial, "shell", "am", "start", "-a",
                                     "android.intent.action.VIEW", "-d", url)
            self.say("connected", "USB: Quest connected. The gallery is open in the headset's browser."
                     if self.open_browser else "USB: Quest connected (open the gallery from a bookmark).")
        elif any(st == "unauthorized" for st, _ in devices.values()):
            self.say("unauthorized", "USB: put the headset on and allow USB debugging "
                                     "(tick 'Always allow from this computer').")
        elif any("permissions" in st or "permissions" in rest for st, rest in devices.values()):
            self.say("no-permission", "USB: no permission to talk to the headset. "
                                      "Install udev rules (pacman -S android-udev) and replug.")
        elif quest_on_usb():
            self.say("no-debugging", "USB: a Quest is plugged in but USB debugging is off "
                                     "(needs developer mode, once). Wi-Fi works without it.")
        else:
            self.say("waiting", "USB: waiting for the Quest to be plugged in.")

    def cleanup(self):
        if self.adb:
            for serial in list(self.ready):
                try:
                    self.adb_run("-s", serial, "reverse", "--remove", f"tcp:{self.port}", timeout=5)
                except (OSError, subprocess.SubprocessError):
                    pass


# --------------------------------------------------------------------------- main


class Share:
    def __init__(self, args):
        self.args = args
        self.verbose = args.verbose
        self.state = State(args.state_dir)
        self.token = self.state.token()
        self.library = Library(self.state, args.folder)
        self.stopping = threading.Event()
        self.urls: dict[str, str] = {}
        self.last_seen = time.monotonic()

    def touch(self):
        self.last_seen = time.monotonic()

    def url(self, kind: str) -> str:
        return self.urls.get(kind, "")

    def listing(self, kind: str) -> dict:
        photos = self.library.scan()
        self.state.update_status(photos=len(photos))
        return {
            "version": VERSION,
            "photos": photos,
            # Offered on the USB page so you can open + bookmark the Wi-Fi link once.
            "wifi_url": self.url("wifi") if kind == "usb" else "",
        }

    def serve(self, kind: str, host: str, port: int, tls: ssl.SSLContext | None) -> Server:
        handler = type(f"{kind}Handler", (Handler,), {"share": self, "kind": kind})
        srv = Server((host, port), handler)
        srv.tls = tls
        threading.Thread(target=srv.serve_forever, name=kind, daemon=True).start()
        return srv


def main() -> int:
    ap = argparse.ArgumentParser(description="Show Stereowright 3D photos on a Meta Quest (USB and/or home Wi-Fi).")
    ap.add_argument("--mode", choices=["both", "usb", "wifi"], default="both")
    ap.add_argument("--folder", action="append", type=Path, default=[],
                    help="extra folder with Stereowright outputs (repeatable)")
    ap.add_argument("--usb-port", type=int, default=8780)
    ap.add_argument("--wifi-port", type=int, default=8781)
    ap.add_argument("--wifi-host", help="address to listen on for Wi-Fi (default: this PC's LAN address)")
    ap.add_argument("--no-open", action="store_true", help="don't open the gallery on the headset automatically")
    ap.add_argument("--adb", default=shutil.which("adb"), help=argparse.SUPPRESS)
    ap.add_argument("--state-dir", type=Path, default=data_dir() / "share", help=argparse.SUPPRESS)
    ap.add_argument("--idle-hours", type=float, default=0,
                    help="stop after this many hours without the headset asking for anything (0 = never)")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()

    share = Share(args)
    st = share.state
    st.clear_status()
    st.update_status(pid=os.getpid(), started=int(time.time()), mode=args.mode)
    servers: list[Server] = []
    usb: UsbLink | None = None

    if args.mode in ("both", "usb"):
        servers.append(share.serve("usb", "127.0.0.1", args.usb_port, None))
        share.urls["usb"] = f"http://localhost:{args.usb_port}/t/{share.token}/"
        usb = UsbLink(share, args.adb, args.usb_port, not args.no_open)
        log(f"USB: serving on this computer only ({share.urls['usb']})")

    if args.mode in ("both", "wifi"):
        lan = (args.wifi_host, f"{args.wifi_host}/32") if args.wifi_host else lan_address()
        if lan is None:
            msg = "Wi-Fi: no home-network address found on this computer."
            log(msg)
            st.update_status(wifi={"url": "", "message": msg})
        else:
            ip, cidr = lan
            cert, key = ensure_cert(st, ip)
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.minimum_version = ssl.TLSVersion.TLSv1_2
            ctx.load_cert_chain(cert, key)
            servers.append(share.serve("wifi", ip, args.wifi_port, ctx))
            share.urls["wifi"] = f"https://{ip}:{args.wifi_port}/t/{share.token}/"
            fw = firewall_hint(args.wifi_port, cidr)
            msg = f"Wi-Fi: open {share.urls['wifi']} in the Quest browser (bookmark it)."
            log(msg)
            if fw:
                log("Wi-Fi: your firewall blocks the headset. Allow your home network once with:")
                log("    " + fw)
            st.update_status(wifi={"url": share.urls["wifi"], "message": msg, "firewall": fw or ""})

    if not servers:
        return 1
    if usb:
        usb.start()

    def stop(*_):
        share.stopping.set()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    count = len(share.library.scan())
    st.update_status(photos=count)
    log(f"Sharing {count} 3D photos. Ctrl+C to stop.")
    while not share.stopping.wait(5):
        if args.idle_hours and time.monotonic() - share.last_seen > args.idle_hours * 3600:
            log("Stopping: idle.")
            break
    for s in servers:
        s.shutdown()
        s.server_close()
    if usb:
        usb.cleanup()
    st.clear_status()
    log("Stopped.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
