"""仅用于本地协议验收；测试账户与数据不用于生产。"""
from pathlib import Path

from cheroot.wsgi import Server
from wsgidav.fs_dav_provider import FilesystemProvider
from wsgidav.prop_man.property_manager import ShelvePropertyManager
from wsgidav.wsgidav_app import WsgiDAVApp

for name in ("basic", "digest"):
    directory = Path("/data") / name
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "seed.txt").write_text(f"{name} seed\n", encoding="utf-8")
Path("/properties").mkdir(exist_ok=True)
app = WsgiDAVApp({
    "provider_mapping": {
        "/basic": FilesystemProvider("/data/basic"),
        "/digest": FilesystemProvider("/data/digest"),
    },
    "property_manager": ShelvePropertyManager("/properties/dav"),
    "http_authenticator": {"accept_basic": True, "accept_digest": True, "default_to_digest": True},
    "simple_dc": {"user_mapping": {"*": {"tester": {"password": "fixture-password"}}}},
    "verbose": 1,
})
Server(("0.0.0.0", 8080), app).start()
