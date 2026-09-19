// Positive interoperability peer using Node's built-in WebSocket, without npm.
"use strict";

const [url, message = "node-peer-text"] = process.argv.slice(2);
const parsed = new URL(url);
if (parsed.protocol !== "ws:" || parsed.hostname !== "127.0.0.1") {
  throw new Error("expected a loopback ws:// URL");
}
const timer = setTimeout(() => {
  console.error("WebSocket peer deadline expired");
  process.exit(2);
}, 5000);
const socket = new WebSocket(url);
let received = false;
socket.addEventListener("open", () => socket.send(message));
socket.addEventListener("message", (event) => {
  if (received || event.data !== message) {
    console.error("unexpected echo", event.data);
    process.exit(3);
  }
  received = true;
  socket.close(1000, "done");
});
socket.addEventListener("close", (event) => {
  clearTimeout(timer);
  console.log(JSON.stringify({
    received, code: event.code, reason: event.reason, clean: event.wasClean,
  }));
  process.exit(received && event.code === 1000 && event.wasClean ? 0 : 4);
});
socket.addEventListener("error", (event) => {
  console.error("WebSocket error", event.message);
  process.exit(5);
});
