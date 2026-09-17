import RFB from "./rfb.js";

const status = document.querySelector("#status");
const screen = document.querySelector("#screen");
const path = new URLSearchParams(window.location.search).get("path");

if (!path) {
  status.textContent = "Missing viewer path";
  throw new Error("missing viewer path");
}

const scheme = window.location.protocol === "https:" ? "wss" : "ws";
const socket = `${scheme}://${window.location.host}/${path}`;
const rfb = new RFB(screen, socket);
rfb.scaleViewport = true;
rfb.resizeSession = false;
rfb.background = "#111";

rfb.addEventListener("connect", () => { status.textContent = "Connected"; });
rfb.addEventListener("disconnect", (event) => {
  status.textContent = event.detail.clean ? "Disconnected" : "Connection lost";
});
rfb.addEventListener("credentialsrequired", () => {
  status.textContent = "Unexpected VNC authentication request";
  rfb.disconnect();
});

document.querySelector("#cad").addEventListener("click", () => rfb.sendCtrlAltDel());
