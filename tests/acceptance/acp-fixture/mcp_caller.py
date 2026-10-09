#!/usr/bin/env python3
"""External MCP caller for the Linux Node/daemon/exporter boundary fixture.

The exporter library is real; Kit boot and host stock registration are supplied
by the fixture_caller test harness. This is not macOS/stock-SBX qualification.
"""
import json
import socket
import sys
import time
import uuid


def main():
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(15)
        connection.connect(sys.argv[1])
        wire = connection.makefile("rwb", buffering=0)
        request_id = 0

        def rpc(method, params):
            nonlocal request_id
            request_id += 1
            wire.write((json.dumps(dict(jsonrpc="2.0", id=request_id, method=method, params=params)) + "\n").encode())
            while True:
                line = wire.readline(1_048_577)
                assert line and len(line) <= 1_048_576, "missing/oversize MCP reply"
                reply = json.loads(line)
                if reply.get("id") == request_id:
                    assert "error" not in reply, reply
                    return reply["result"]

        initialized = rpc("initialize", dict(protocolVersion="2025-03-26", capabilities={}, clientInfo=dict(name="external-acp-fixture", version="1")))
        assert initialized["serverInfo"]["name"] == "marsh-acp-export", initialized
        wire.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
        tools = rpc("tools/list", {})["tools"]
        assert [tool["name"] for tool in tools] == ["fixture_tool"], tools

        def call(**args):
            result = rpc("tools/call", dict(name="fixture_tool", arguments=args))
            assert not result.get("isError"), result
            value = result["structuredContent"]
            assert value["ok"], value
            return value["result"]

        def ask(text):
            key = str(uuid.uuid4())
            receipt = call(action="ask", text=text, key=key)
            assert "status" not in receipt and receipt["turn_id"] and isinstance(receipt["start_cursor"], int), receipt
            return receipt, key

        def collect(receipt):
            cursor, chunks = receipt["start_cursor"], []
            deadline = time.monotonic() + 15
            while True:
                page = call(action="status", turn_id=receipt["turn_id"], cursor=cursor)
                assert not page["updates_lost"], page
                if not page["updates"]:
                    assert page["next_cursor"] == cursor, page
                for update in page["updates"]:
                    assert update["turn_id"] == receipt["turn_id"], update
                    chunks.append(update["update"]["content"]["text"])
                cursor = page["next_cursor"]
                if not page["turn_active"] and not page["more_updates"]:
                    return chunks, page
                assert time.monotonic() < deadline, "turn did not complete"
                time.sleep(.02)

        first, key = ask("slow-6")
        chunks, page = collect(first)
        assert chunks == ["fixture:slow-6", *(f"u{n};" for n in range(6))], chunks
        assert call(action="ask", text="slow-6", key=key) == first
        # Idle polling is observed on a held turn (after its echo the agent writes
        # nothing until cancelled), not by racing a paced one: a stalled machine
        # can poll after the paced turn is over.
        held, _ = ask("hold")
        cursor, idle = held["start_cursor"], 0
        deadline = time.monotonic() + 15
        while idle < 4:
            page = call(action="status", turn_id=held["turn_id"], cursor=cursor)
            assert page["turn_active"], page
            if not page["updates"]:
                assert page["next_cursor"] == cursor, page
                idle += 1
            cursor = page["next_cursor"]
            assert time.monotonic() < deadline, "held turn never went idle"
            time.sleep(.005)
        call(action="cancel")
        chunks, page = collect(held)
        assert page["last_stop_reason"] == "cancelled", page
        tail, _ = ask("cancel-burst")
        call(action="cancel")
        chunks, page = collect(tail)
        assert chunks == ["fixture:cancel-burst", *(f"c{n};" for n in range(200))], chunks
        assert page["last_stop_reason"] == "cancelled", page
        late, _ = ask("late")
        chunks, page = collect(late)
        assert chunks == ["fixture:late"], chunks
        deadline = time.monotonic() + 5
        while page["session_out_of_turn_updates"] != 1:
            assert time.monotonic() < deadline, page
            page = call(action="status", turn_id=late["turn_id"])
            time.sleep(.01)
        oversized, _ = ask("oversize-update")
        deadline = time.monotonic() + 5
        while True:
            page = call(action="status", turn_id=oversized["turn_id"])
            if not page["turn_active"]:
                assert page["updates_lost"], page
                break
            assert time.monotonic() < deadline
            time.sleep(.01)
        older = call(action="status", turn_id=first["turn_id"], cursor=first["start_cursor"])
        assert not older["updates_lost"] and older["last_stop_reason"] == "end_turn", older
        print(json.dumps(dict(external_mcp_caller="passed", idle_polls=idle, cancel_tail_chunks=201, old_turn_preserved=first["turn_id"], out_of_turn_updates=1)))


if __name__ == "__main__":
    main()
