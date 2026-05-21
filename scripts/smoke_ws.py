"""
Smoke test for vtcast-relay signaling.

Flow:
  1. POST /api/new-room → get a room code
  2. Open publisher WS, send Hello → expect Welcome
  3. Open subscriber WS, send Hello → expect Welcome + PeerJoined on publisher
  4. Subscriber disconnects → publisher receives PeerLeft

The SDP/ICE path moved from peer-to-peer relay to client<->SFU negotiation,
so a real WebRTC client is required to exercise it. See test_rtc.html for the
browser-based negotiation check.

Run:  python scripts/smoke_ws.py
"""
import asyncio
import json
import urllib.request

import websockets

BASE = "http://localhost:17239"
WS = "ws://localhost:17239/ws"


def new_room():
    with urllib.request.urlopen(f"{BASE}/api/new-room") as r:
        return json.loads(r.read())["code"]


async def expect(ws, kind, label):
    raw = await asyncio.wait_for(ws.recv(), timeout=2)
    msg = json.loads(raw)
    assert msg["type"] == kind, f"{label}: expected {kind}, got {msg}"
    return msg


async def send(ws, seq, msg):
    await ws.send(json.dumps({"seq": seq, **msg}))


async def main():
    code = new_room()
    print(f"room: {code}")

    pub = await websockets.connect(WS)
    await send(pub, 1, {"type": "hello", "protocol_version": 1,
                        "role": "publisher", "room": code})
    welcome_pub = await expect(pub, "welcome", "publisher hello")
    pub_id = welcome_pub["peer_id"]
    assert welcome_pub["room_state"]["peers"] == [], "room should be empty before publisher"
    print(f"publisher  welcomed as peer {pub_id}")

    sub = await websockets.connect(WS)
    await send(sub, 1, {"type": "hello", "protocol_version": 1,
                        "role": "subscriber", "room": code})
    welcome_sub = await expect(sub, "welcome", "subscriber hello")
    sub_id = welcome_sub["peer_id"]
    sub_sees = welcome_sub["room_state"]["peers"]
    assert len(sub_sees) == 1 and sub_sees[0]["peer_id"] == pub_id, \
        f"subscriber should see publisher in room_state, got {sub_sees}"
    print(f"subscriber welcomed as peer {sub_id}, sees publisher {pub_id}")

    joined = await expect(pub, "peer_joined", "publisher waits for subscriber")
    assert joined["peer"]["peer_id"] == sub_id
    print(f"publisher  observed peer_joined for {sub_id}")

    await sub.close()
    left = await expect(pub, "peer_left", "publisher waits for subscriber leave")
    assert left["peer_id"] == sub_id
    print(f"publisher  observed peer_left for {sub_id}")

    await pub.close()
    print("\nALL CHECKS PASSED")


asyncio.run(main())
