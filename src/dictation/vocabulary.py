"""Bounded word lists and the matching SentencePiece vocabulary for native biasing."""

import math
from pathlib import Path
import re
import struct


def terms(value):
    if len(value) > 20000:
        raise ValueError("Word list is too long")
    result = []
    seen = set()
    for item in re.split(r"[,\n\r]+", value):
        item = " ".join(item.split())
        if not item or item.casefold() in seen:
            continue
        if len(item) > 80 or any(ord(character) < 32 for character in item):
            raise ValueError("Each word or phrase must be at most 80 characters without control characters")
        result.append(item)
        seen.add(item.casefold())
    if len(result) > 100:
        raise ValueError("Use at most 100 words or phrases per list")
    return result


def hotwords(value):
    result = []
    for term in terms(value):
        if any(word.startswith(":") for word in term.split()):
            raise ValueError("Vocabulary words cannot begin with a colon")
        for variant in (term, term.lower(), term[:1].upper() + term[1:]):
            if len(result) < 100 and variant not in result:
                result.append(variant)
    return "\n".join(result)


def _varint(data, offset):
    value = 0
    for shift in range(0, 70, 7):
        if offset >= len(data):
            raise ValueError("Truncated tokenizer integer")
        byte = data[offset]
        offset += 1
        value |= (byte & 127) << shift
        if not byte & 128:
            return value, offset
    raise ValueError("Invalid tokenizer integer")


def _fields(data):
    offset = 0
    while offset < len(data):
        key, offset = _varint(data, offset)
        field, wire = key >> 3, key & 7
        if not field:
            raise ValueError("Invalid tokenizer field")
        if wire == 0:
            value, offset = _varint(data, offset)
        elif wire in (1, 2, 5):
            if wire == 2:
                length, offset = _varint(data, offset)
            else:
                length = 8 if wire == 1 else 4
            if offset + length > len(data):
                raise ValueError("Truncated tokenizer field")
            value = data[offset:offset + length]
            offset += length
        else:
            raise ValueError("Unsupported tokenizer field")
        yield field, wire, value


def sentencepiece_vocabulary(data, token_path):
    """Export original piece scores; never guess BPE ranks from token IDs."""
    if not 0 < len(data) <= 1_000_000:
        raise ValueError("Unexpected tokenizer size")
    pieces = {}
    for number, wire, message in _fields(data):
        if number != 1 or wire != 2:
            continue
        fields = {(key, kind): value for key, kind, value in _fields(message)}
        try:
            piece = fields[(1, 2)].decode("utf-8")
            score = struct.unpack("<f", fields[(2, 5)])[0]
        except (KeyError, UnicodeError, struct.error) as error:
            raise ValueError("Invalid tokenizer piece") from error
        if not piece or any(character.isspace() for character in piece) or not math.isfinite(score):
            raise ValueError("Invalid tokenizer piece or score")
        if piece in pieces:
            raise ValueError("Duplicate tokenizer piece")
        pieces[piece] = score
    tokens = {line.rsplit(" ", 1)[0] for line in Path(token_path).read_text().splitlines()}
    if set(pieces) != tokens - {"<blk>"}:
        raise ValueError("Tokenizer vocabulary does not match the installed speech model")
    return "".join(f"{piece}\t{score:.9g}\n" for piece, score in pieces.items())
