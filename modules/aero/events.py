import copy
import json
import queue
import time
import uuid


class ResponseEvents:
    def __init__(self, model):
        self.response = {
            "id": "resp_aero_" + uuid.uuid4().hex,
            "object": "response",
            "created_at": int(time.time()),
            "model": model,
            "status": "in_progress",
            "output": [],
            "usage": None,
        }
        self.frames = queue.SimpleQueue()
        self.tool_frames = []
        self.deferring_tool = False
        self.sequence = 0
        self.started = False
        self.current = {}
        self.indices = {}
        self.next_index = 0

    def event(self, kind, **fields):
        value = {"type": kind, "sequence_number": self.sequence, **fields}
        self.sequence += 1
        payload = (
            "event: "
            + kind
            + "\ndata: "
            + json.dumps(value, ensure_ascii=False)
            + "\n\n"
        ).encode()
        if self.deferring_tool:
            self.tool_frames.append(payload)
        else:
            self.frames.put(payload)

    def start(self):
        if not self.started:
            self.started = True
            self.event("response.created", response=self.response.copy())

    def begin_model(self):
        self.start()
        self.current = {}
        self.indices = {}

    def added(self, item):
        identifier = item.get("id") or "item_aero_" + uuid.uuid4().hex
        wire = copy.deepcopy(item)
        wire["id"] = (
            identifier
            if identifier.startswith("item_aero_")
            else "item_aero_" + uuid.uuid4().hex
        )
        wire["status"] = "in_progress"
        if wire["type"] == "message":
            wire["phase"] = "commentary"
            wire["content"] = []
        elif wire["type"] == "function_call":
            wire["arguments"] = ""
        elif wire["type"] == "custom_tool_call":
            wire["input"] = ""
        slot = {"item": wire, "index": self.next_index, "parts": {}}
        self.next_index += 1
        self.current[identifier] = slot
        self.event("response.output_item.added", output_index=slot["index"], item=wire)
        return slot

    def part(self, slot, index, value):
        parts = slot["parts"]
        if index not in parts:
            part = copy.deepcopy(value)
            if part.get("type") == "output_text":
                part["text"] = ""
                part.setdefault("annotations", [])
            parts[index] = {"value": part, "text": ""}
            self.event(
                "response.content_part.added",
                item_id=slot["item"]["id"],
                output_index=slot["index"],
                content_index=index,
                part=part,
            )
        return parts[index]

    def upstream(self, event):
        kind = event.get("type", "")
        if kind == "response.output_item.added":
            item = event["item"]
            if item["type"] in (
                "function_call",
                "custom_tool_call",
                "tool_search_call",
                "reasoning",
            ):
                return
            slot = self.added(item)
            self.indices[event["output_index"]] = slot
            return
        slot = self.indices.get(event.get("output_index"))
        if slot is None:
            slot = next(
                (
                    value
                    for key, value in self.current.items()
                    if key == event.get("item_id")
                ),
                None,
            )
        if slot is None:
            return
        if kind == "response.content_part.added":
            self.part(slot, event["content_index"], event["part"])
        elif kind == "response.output_text.delta":
            part = self.part(
                slot, event.get("content_index", 0), {"type": "output_text"}
            )
            part["text"] += event["delta"]
            self.event(
                kind,
                item_id=slot["item"]["id"],
                output_index=slot["index"],
                content_index=event.get("content_index", 0),
                delta=event["delta"],
            )
        elif kind.startswith(
            (
                "response.image_generation_call.",
                "response.audio.",
                "response.output_audio.",
            )
        ):
            fields = {
                key: value
                for key, value in event.items()
                if key not in ("type", "sequence_number")
            }
            fields.update(item_id=slot["item"]["id"], output_index=slot["index"])
            self.event(kind, **fields)

    def items(self, items, final=False):
        result = []
        for original in items:
            self.deferring_tool = original["type"] in (
                "function_call",
                "custom_tool_call",
                "tool_search_call",
            )
            slot = self.current.get(original.get("id"))
            if slot is None:
                slot = self.added(original)
            item = copy.deepcopy(original)
            item.update(id=slot["item"]["id"], status="completed")
            if item["type"] == "message":
                item["phase"] = "final_answer" if final else "commentary"
                for index, part in enumerate(item["content"]):
                    progress = self.part(slot, index, part)
                    if part["type"] == "output_text":
                        part.setdefault("annotations", [])
                        text = part.get("text", "")
                        remaining = text[len(progress["text"]) :]
                        if remaining:
                            self.event(
                                "response.output_text.delta",
                                item_id=item["id"],
                                output_index=slot["index"],
                                content_index=index,
                                delta=remaining,
                            )
                        self.event(
                            "response.output_text.done",
                            item_id=item["id"],
                            output_index=slot["index"],
                            content_index=index,
                            text=text,
                        )
                    self.event(
                        "response.content_part.done",
                        item_id=item["id"],
                        output_index=slot["index"],
                        content_index=index,
                        part=part,
                    )
            elif item["type"] == "function_call":
                self.event(
                    "response.function_call_arguments.delta",
                    item_id=item["id"],
                    output_index=slot["index"],
                    delta=item["arguments"],
                )
                self.event(
                    "response.function_call_arguments.done",
                    item_id=item["id"],
                    output_index=slot["index"],
                    arguments=item["arguments"],
                )
            elif item["type"] == "custom_tool_call":
                self.event(
                    "response.custom_tool_call_input.delta",
                    item_id=item["id"],
                    call_id=item["call_id"],
                    output_index=slot["index"],
                    delta=item["input"],
                )
                self.event(
                    "response.custom_tool_call_input.done",
                    item_id=item["id"],
                    output_index=slot["index"],
                    input=item["input"],
                )
            self.event(
                "response.output_item.done", output_index=slot["index"], item=item
            )
            result.append(item)
        self.deferring_tool = False
        return result

    def finish(self, response):
        if not self.started:
            self.response.update(response, output=[], status="in_progress", usage=None)
            self.start()
            for item in response["output"]:
                self.items([item], final=item.get("phase") == "final_answer")
        for frame in self.tool_frames:
            self.frames.put(frame)
        self.tool_frames.clear()
        self.event("response.completed", response=response)

    def drain(self):
        frames = []
        while not self.frames.empty():
            frames.append(self.frames.get_nowait())
        return b"".join(frames)
