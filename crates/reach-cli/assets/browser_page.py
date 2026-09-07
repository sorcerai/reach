import os
from urllib.parse import urlsplit


def _isolated_document_has_focus(page):
    """Read document.hasFocus() from a native CDP isolated world."""
    cdp = page.context.new_cdp_session(page)
    try:
        # Playwright enables focus emulation for every attached tab.
        cdp.send("Emulation.setFocusEmulationEnabled", {"enabled": False})
        frame_tree = cdp.send("Page.getFrameTree")
        frame = ((frame_tree or {}).get("frameTree") or {}).get("frame") or {}
        frame_id = frame.get("id")
        if not frame_id:
            raise RuntimeError("focused-page check has no main frame")
        world = cdp.send(
            "Page.createIsolatedWorld",
            {"frameId": frame_id, "worldName": "__reach_focus__", "grantUniveralAccess": True},
        )
        context_id = (world or {}).get("executionContextId")
        if not context_id:
            raise RuntimeError("focused-page check has no isolated execution context")
        result = cdp.send(
            "Runtime.evaluate",
            {
                "expression": "document.hasFocus()",
                "contextId": context_id,
                "returnByValue": True,
                "awaitPromise": False,
            },
        )
        return bool((((result or {}).get("result") or {}).get("value")))
    finally:
        try:
            cdp.detach()
        except Exception:
            pass


def current_page(browser, require_focus=True):
    """Return the uniquely focused page from exactly one browser context."""
    contexts = list(browser.contexts)
    if len(contexts) != 1:
        raise RuntimeError("expected exactly one browser context")
    pages = list(contexts[0].pages)
    if not pages:
        raise RuntimeError("browser context has no pages")

    focused = []
    focus_error = None
    for page in pages:
        try:
            if _isolated_document_has_focus(page):
                focused.append(page)
        except Exception as exc:
            focus_error = exc
    if focus_error is not None and len(pages) > 1:
        raise RuntimeError("focused-page check failed") from focus_error
    if len(focused) == 1:
        return focused[0]
    if len(focused) > 1:
        raise RuntimeError("browser has multiple focused pages")
    if not require_focus and len(pages) == 1:
        return pages[0]
    raise RuntimeError("browser has no uniquely focused page")


def page_target_id(page):
    """Return the native CDP target ID for a Playwright page."""
    cdp = page.context.new_cdp_session(page)
    try:
        info = cdp.send("Target.getTargetInfo") or {}
        target_info = info.get("targetInfo") or {}
        target_id = target_info.get("targetId")
        if not isinstance(target_id, str) or not target_id:
            raise RuntimeError("page has no native target identity")
        return target_id
    finally:
        try:
            cdp.detach()
        except Exception:
            pass


def page_loader_id(page):
    """Return the native main-frame loader identity for this document."""
    cdp = page.context.new_cdp_session(page)
    try:
        frame_tree = cdp.send("Page.getFrameTree")
        frame = ((frame_tree or {}).get("frameTree") or {}).get("frame") or {}
        loader_id = frame.get("loaderId")
        if not isinstance(loader_id, str) or not loader_id:
            raise RuntimeError("page has no native loader identity")
        return loader_id
    finally:
        try:
            cdp.detach()
        except Exception:
            pass


def page_backend_node_id(page, selector):
    """Resolve a selector to the current native backend node identity."""
    cdp = page.context.new_cdp_session(page)
    try:
        document = cdp.send("DOM.getDocument", {"depth": 0}) or {}
        root = (document.get("root") or {}).get("nodeId")
        if not root:
            raise RuntimeError("document has no native root")
        found = cdp.send(
            "DOM.querySelector", {"nodeId": root, "selector": selector}
        ) or {}
        node_id = found.get("nodeId")
        if not node_id:
            raise RuntimeError("ref target has no native node")
        described = cdp.send("DOM.describeNode", {"nodeId": node_id}) or {}
        backend_id = ((described.get("node") or {}).get("backendNodeId"))
        if not isinstance(backend_id, int) or backend_id <= 0:
            raise RuntimeError("ref target has no native backend identity")
        return backend_id
    finally:
        try:
            cdp.detach()
        except Exception:
            pass


def verify_cdp_profile(browser, expected):
    if not expected:
        return
    try:
        cdp = browser.new_browser_cdp_session()
        command_line = cdp.send("Browser.getBrowserCommandLine").get("arguments", [])
        actual = [
            arg.split("=", 1)[1]
            for arg in command_line
            if isinstance(arg, str) and arg.startswith("--user-data-dir=")
        ]
    except Exception as exc:
        raise RuntimeError(f"profile affinity check failed: {exc}")
    if len(actual) != 1 or os.path.realpath(actual[0]) != os.path.realpath(expected):
        found = actual[0] if actual else "<unknown>"
        raise RuntimeError(
            f"profile affinity check failed: expected {expected}, found {found}"
        )


def browser_origin(url):
    parsed = urlsplit(url)
    if parsed.scheme not in ("http", "https") or not parsed.hostname:
        return None
    host = parsed.hostname.encode("idna").decode("ascii").lower()
    if ":" in host:
        host = "[" + host + "]"
    port = parsed.port
    suffix = "" if port is None or port == {"http": 80, "https": 443}[parsed.scheme] else ":" + str(port)
    return parsed.scheme + "://" + host + suffix


class NavigationGuard:
    """Fence every top-level request, including redirect hops, before network dispatch."""

    def __init__(self, page, allowed_origins):
        self.page = page
        self.allowed = None if allowed_origins is None else set(allowed_origins)
        self.blocked = False
        self.cdp = None
        if self.allowed is None:
            return
        self.cdp = page.context.new_cdp_session(page)
        self.frame = self.cdp.send("Page.getFrameTree")["frameTree"]["frame"]["id"]
        self.cdp.on("Fetch.requestPaused", self._paused)
        try:
            self.cdp.send("Fetch.enable", {"patterns": [
                {"urlPattern": "*", "resourceType": "Document", "requestStage": "Request"}
            ]})
        except Exception:
            self.cdp.detach()
            raise

    def _paused(self, event):
        denied = False
        if event.get("frameId") == self.frame:
            try:
                denied = browser_origin(event["request"]["url"]) not in self.allowed
            except (KeyError, ValueError, UnicodeError):
                denied = True
        request = {"requestId": event["requestId"]}
        if denied:
            self.blocked = True
            request["errorReason"] = "BlockedByClient"
            self.cdp.send("Fetch.failRequest", request)
        else:
            self.cdp.send("Fetch.continueRequest", request)

    def check(self):
        if self.allowed is not None and (
            self.blocked or browser_origin(self.page.url) not in self.allowed
        ):
            raise RuntimeError("browser origin is outside this lease's allowed origins")

    def close(self):
        if self.cdp is not None:
            try:
                self.cdp.send("Fetch.disable")
            finally:
                self.cdp.detach()
