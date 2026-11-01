"""Extract the real native ring reservation primitive and its type dependencies."""
import re


def native_channel_helpers(source):
    def block(pattern):
        match = re.search(pattern, source, re.M)
        assert match, pattern
        cursor = source.index("{", match.start()) + 1
        level = 1
        while level:
            level += (source[cursor] == "{") - (source[cursor] == "}")
            cursor += 1
        return source[match.start():cursor]

    types = ["SubmissionError", "G17ChannelAbiTarget", "G17PCachedChannelStateLayout",
             "G17PUncachedChannelStateLayout", "G17PChannelAbiLayout", "G17PChannelWritePlan"]
    declarations = ["#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n" + block(
        r"^pub\(crate\) (?:struct|enum) " + name + r"\s+"
    ) for name in types]
    abi = re.search(r"^pub\(crate\) const G17P_CHANNEL_ABI: .*?;", source, re.M | re.S)
    assert abi
    return "\n".join(declarations + [abi.group()] + [block(
        r"^(?:pub\(crate\) )?(?:const )?fn " + name + r"\("
    ) for name in ["g17_channel_abi", "ring_next", "prepare_g17p_channel_write"]])
