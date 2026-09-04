const endpoint = process.env.VDESK_ENDPOINT;
const token = process.env.VDESK_VIEWER_TOKEN;
if (!endpoint || !token) throw new Error("VDESK_ENDPOINT and VDESK_VIEWER_TOKEN are required");

const socket = new WebSocket(
  `${endpoint.replace(/^http/, "ws")}/viewer/ws?token=${encodeURIComponent(token)}`,
  ["binary"],
);
socket.binaryType = "arraybuffer";

let phase = 0;
let buffered = new Uint8Array();
const deadline = setTimeout(() => process.exit(9), 4_000);

function append(bytes) {
  const joined = new Uint8Array(buffered.length + bytes.length);
  joined.set(buffered);
  joined.set(bytes, buffered.length);
  buffered = joined;
}

function take(length) {
  if (buffered.length < length) return null;
  const value = buffered.slice(0, length);
  buffered = buffered.slice(length);
  return value;
}

socket.onerror = () => process.exit(8);
socket.onmessage = (event) => {
  append(new Uint8Array(event.data));
  for (;;) {
    if (phase === 0) {
      const version = take(12);
      if (!version) return;
      socket.send(version);
      phase = 1;
    } else if (phase === 1) {
      if (buffered.length < 1) return;
      const count = buffered[0];
      if (buffered.length < count + 1) return;
      take(count + 1);
      socket.send(Uint8Array.of(1));
      phase = 2;
    } else if (phase === 2) {
      const securityResult = take(4);
      if (!securityResult) return;
      if (new DataView(securityResult.buffer).getUint32(0) !== 0) process.exit(7);
      socket.send(Uint8Array.of(1));
      phase = 3;
    } else {
      if (buffered.length < 24) return;
      const nameLength = new DataView(buffered.buffer, buffered.byteOffset).getUint32(20);
      if (buffered.length < 24 + nameLength) return;
      take(24 + nameLength);
      socket.send(Uint8Array.of(5, 1, 0, 200, 0, 200));
      socket.send(Uint8Array.of(5, 0, 0, 200, 0, 200));
      setTimeout(() => {
        clearTimeout(deadline);
        socket.close();
        process.exit(0);
      }, 150);
      return;
    }
  }
};
