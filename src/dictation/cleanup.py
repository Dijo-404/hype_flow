"""Literal, reversible transcript cleanup. No language model or guessed spelling."""

import math
import re

from .vocabulary import terms


DEFAULT_FILTERS = "um, uh, erm, umm, uhh"
_PROTECTED = re.compile(r"(https?://\S+|www\.\S+|[\w.+-]+@[\w.-]+\.[A-Za-z]{2,}|`[^`\n]*`|\"[^\"\n]*\"|(?<!\w)'[^'\n]*'(?!\w))")
_WORD_EDGE = r"[\w'’\-]"


def corrections(value):
    if len(value) > 33000:
        raise ValueError("Correction list is too long")
    result = {}
    for number, line in enumerate(value.splitlines(), 1):
        if not line.strip():
            continue
        source, separator, target = line.partition("=>")
        source, target = " ".join(source.split()), " ".join(target.split())
        if not separator or not source or not target or not re.search(r"\w", source):
            raise ValueError(f"Correction line {number}: use heard words => replacement")
        if len(source) > 160 or len(target) > 160 or any(ord(character) < 32 for character in line):
            raise ValueError(f"Correction line {number} is too long or contains control characters")
        key = source.casefold()
        if key in result:
            raise ValueError(f"Duplicate correction on line {number}")
        result[key] = target
    if len(result) > 100:
        raise ValueError("Use at most 100 correction rules")
    return result


def _pattern(values):
    choices = [r"\s+".join(re.escape(word) for word in value.split())
               for value in sorted(values, key=len, reverse=True)]
    return rf"(?<!{_WORD_EDGE})(?:{'|'.join(choices)})(?!{_WORD_EDGE})"


def _replace(text, mapping):
    if not mapping:
        return text
    return re.sub(_pattern(mapping), lambda match: mapping[" ".join(match[0].split()).casefold()], text, flags=re.IGNORECASE)


def _key(word):
    return "".join(character for character in word.casefold() if character.isalnum())


def soundex(word):
    """American Soundex of an ASCII word."""
    codes = {**dict.fromkeys("bfpv", "1"), **dict.fromkeys("cgjkqsxz", "2"), **dict.fromkeys("dt", "3"),
             "l": "4", **dict.fromkeys("mn", "5"), "r": "6"}
    word = word.lower()
    result, previous = word[0].upper(), codes.get(word[0], "")
    for character in word[1:]:
        code = codes.get(character, "")
        if code and code != previous:
            result += code
        if character not in "hw":
            previous = code
    return (result + "000")[:4]


def _distance(first, second):
    previous = list(range(len(second) + 1))
    for row, left in enumerate(first, 1):
        current = [row]
        for column, right in enumerate(second, 1):
            current.append(min(previous[column] + 1, current[-1] + 1, previous[column - 1] + (left != right)))
        previous = current
    return previous[-1]


def _closest(candidate, keys, tolerance):
    if not candidate.isascii() or not candidate.isalnum() or len(candidate) > 50:
        return None
    best, best_score = None, math.inf
    for key, word in keys:
        longest = max(len(candidate), len(key))
        # At most a 25% length difference, so "openaigpt" cannot match "openai".
        if abs(len(candidate) - len(key)) > max(longest * 0.25, 2):
            continue
        score = _distance(candidate, key) / longest
        if candidate.isalpha() and key.isalpha() and soundex(candidate) == soundex(key):
            score *= 0.3  # Words that sound alike get a strong boost.
        if score < tolerance and score < best_score:
            best, best_score = word, score
    return (best, best_score) if best else None


def _edges(word):
    start, end = 0, len(word)
    while start < end and not word[start].isalnum():
        start += 1
    while end > start and not word[end - 1].isalnum():
        end -= 1
    return word[:start], word[end:]


def _cased(original, replacement):
    letters = [character for character in original if character.isalpha()]
    if letters and all(character.isupper() for character in letters) and len(letters) > 1:
        return replacement.upper()
    if letters and letters[0].isupper():
        return replacement[:1].upper() + replacement[1:]
    return replacement


def near_misses(text, vocabulary, tolerance):
    """Handy's custom-word matcher: snap misheard words, or up to three split words, to vocabulary terms."""
    keys = [(key, word) for word in vocabulary for key in {_key(word), _key(word.replace("&", " and "))}
            if key and key.isascii() and key.isalnum()]
    if not keys:
        return text
    words, result, index = text.split(), [], 0
    while index < len(words):
        best = None
        for size in (3, 2, 1):
            group = words[index:index + size]
            # Never join across punctuation: "Charge B, che" stops at the comma.
            if len(group) < size or any(_edges(word)[1] for word in group[:-1]):
                continue
            match = _closest("".join(_key(word) for word in group), keys, tolerance)
            if match and (best is None or match[1] < best[2]):
                best = (size, match[0], match[1])
        if best:
            size, replacement, _ = best
            group = words[index:index + size]
            result.append(_edges(group[0])[0] + _cased(group[0], replacement) + _edges(group[-1])[1])
            index += size
        else:
            result.append(words[index])
            index += 1
    return " ".join(result)


def process(text, values):
    vocabulary = {term.casefold(): term for term in terms(values.get("prompt", ""))}
    replacements = corrections(values.get("corrections", ""))
    blocked = terms(values.get("blocked_words", DEFAULT_FILTERS))
    removed = False
    pieces = _PROTECTED.split(text)
    for index in range(0, len(pieces), 2):
        plain = pieces[index]
        if values.get("auto_correct", True):
            plain = _replace(plain, vocabulary)
            plain = _replace(plain, replacements)
        # After the literal rules, so an explicit correction always wins over a guess.
        if values.get("fuzzy_vocabulary", True) and vocabulary and plain.strip():
            lead, trail = plain[:len(plain) - len(plain.lstrip())], plain[len(plain.rstrip()):]
            plain = lead + near_misses(plain, list(vocabulary.values()), values.get("fuzzy_tolerance", 0.18)) + trail
        if values.get("filter_words", True) and blocked:
            pattern = rf"(?:,\s*)?{_pattern(blocked)}(?:\s*[,;:])?"

            def remove(match):
                nonlocal removed
                word = match[0].strip(" ,;:")
                # Keep acronyms such as UM unless the user explicitly listed that casing.
                if word.isupper() and len(word) > 1 and word not in blocked:
                    return match[0]
                removed = True
                return " "

            plain = re.sub(pattern, remove, plain, flags=re.IGNORECASE)
        if values.get("remove_repetitions", False):
            # Consecutive words only. Never guess which entire phrase was a verbal restart.
            plain = re.sub(r"\b([\w’'-]+)(?:\s+\1\b)+", r"\1", plain, flags=re.IGNORECASE)
        plain = re.sub(r"[ \t]+", " ", plain)
        plain = re.sub(r" +([,.;:!?])", r"\1", plain)
        pieces[index] = plain
    result = "".join(pieces).strip()
    result = re.sub(r"^[,;:]\s*", "", result)
    if removed and not re.search(r"\w", result):
        return ""
    return result
