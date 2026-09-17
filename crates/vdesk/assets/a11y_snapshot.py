#!/usr/bin/python3
"""Emit a bounded, flat AT-SPI snapshot for vdesk's native service."""

import json
import signal

import pyatspi


MAX_NODES = 800
MAX_DEPTH = 48
MAX_TEXT = 2_000


def clean(value, limit=512):
    return str(value or "").replace("\x00", "")[:limit]


def states(node):
    try:
        return [clean(pyatspi.stateToString(value), 64) for value in node.getState().getStates()]
    except Exception:
        return []


def actions(node):
    try:
        interface = node.queryAction()
        return [clean(interface.getName(index), 128) for index in range(interface.nActions)]
    except Exception:
        return []


def text(node):
    try:
        interface = node.queryText()
        return clean(interface.getText(0, min(interface.characterCount, MAX_TEXT)), MAX_TEXT)
    except Exception:
        return ""


def bounds(node):
    try:
        value = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
        if value.width <= 0 or value.height <= 0:
            return None
        return {"x": value.x, "y": value.y, "width": value.width, "height": value.height}
    except Exception:
        return None


def main():
    signal.alarm(5)
    desktop = pyatspi.Registry.getDesktop(0)
    pending = [(desktop[index], None, 0) for index in range(desktop.childCount - 1, -1, -1)]
    nodes = []
    truncated = False
    while pending:
        node, parent_id, depth = pending.pop()
        if len(nodes) >= MAX_NODES:
            truncated = True
            break
        node_id = f"n{len(nodes) + 1}"
        try:
            role = clean(node.getRoleName(), 128)
            name = clean(node.name)
            description = clean(node.description)
        except Exception:
            continue
        nodes.append(
            {
                "id": node_id,
                "parent_id": parent_id,
                "role": role,
                "name": name,
                "description": description,
                "text": text(node),
                "bounds": bounds(node),
                "states": states(node),
                "actions": actions(node),
            }
        )
        if depth >= MAX_DEPTH:
            truncated = True
            continue
        try:
            children = [node[index] for index in range(node.childCount - 1, -1, -1)]
        except Exception:
            children = []
        pending.extend((child, node_id, depth + 1) for child in children)
    print(json.dumps({"nodes": nodes, "truncated": truncated}, ensure_ascii=False))


if __name__ == "__main__":
    main()
