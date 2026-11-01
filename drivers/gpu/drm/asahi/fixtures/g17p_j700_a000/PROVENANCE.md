# J700 G17P A000 diagnostic instruction fixtures

These fixtures come from the GFX A000 payload inside the J700 T8140 firmware
container below. They are diagnostic evidence only. Runtime code does not load
them or derive submission bytes from them.

- Container: `/Users/dj/Developer/Asahi/research/gpu/fw-re/t8140-a18pro-j700/armfw_g17p.im4p`
- Container SHA-256: `b0f84b9972c72e7c0be96af7bc6e504b7693dd82af551e1a4d2bf408a11df83a`
- A000 FTAB payload offset: `0x99`
- A000 FTAB payload size: `0x185bc0`
- Extracted A000 SHA-256: `3e2821bb40d3abffe30488a06ede5f523cfa47544d6a0ad7654a08be6b5dc30f`
- Mach-O `__TEXT`: VM `0xfffffc0000000000`, file offset `0x4000`
- Mach-O `__DATA`: VM `0xfffffc000004c000`, size `0x134000`
- J700 `gfx-data`: PA `0x10001d7c000`, size `0x134000`

The equal `__DATA` and `gfx-data` sizes establish the link-to-physical mapping
used by the read-only sampler. Link `0xfffffc000004c000` maps to physical
`0x10001d7c000`.

| Fixture | Link VA | Bytes | SHA-256 | Proven use |
| --- | ---: | ---: | --- | --- |
| `boot-main-config-global.hex` | `0xfffffc000001e9bc` | `0x30` | `f156a11eb0bc0e6945a79f958b085b54b727987d5ea0cadf3281a0a78d5e2444` | Store the primary main-config pointer at `0xfffffc0000177030` |
| `type4-selector.hex` | `0xfffffc0000022d98` | `0x1c` | `ff2484143031da081a1cca37aa6bd240b111d76ec5a1408e25648ca6cd5b06b2` | Extract EP message type and branch on type 4 |
| `type4-publish.hex` | `0xfffffc0000022e24` | `0x10` | `ac98d9e0d077510b19c5a047c6aa0fc85662a677d07e5e82b70562c6381e1033` | OR bit 17 into scheduler pending root `0xfffffc0000177190` |
| `type4-common-publish.hex` | `0xfffffc0000022ebc` | `0xc` | `8c447bedf6f551a2bbe971764a754c5d26dd48a235230f70f12a6ddbdb6cf5f6` | Atomically publish the pending bit and wake the scheduler |
| `type4-pending-take.hex` | `0xfffffc00000258e4` | `0x20` | `d3b0dd845ff8ef14eb93346256000fe1fd81735b8f77a5a3e93ee73e652f1794` | Atomically clear and take pending bits before dispatch |
| `scheduler-deferred.hex` | `0xfffffc0000025ea4` | `0x14` | `85f7539bd589bc28bdba97dcf924a9d3ada20dcf5f1ad4b3e2a7c34cbb3eed61` | Merge deferred events at root `+0x50`, link `0xfffffc00001771e0` |
| `type4-thunk.hex` | `0xfffffc000002635c` | `0x28` | `490a1f925ffe302d5bb7c9a0e8fee16900e0829b47b666a6cafa67d8a2f5e3c0` | Read root state `+0x68`, then runtime-main global `0xfffffc0000177030`, `main+0x1b0`, and producer before tail-calling `0xfffffc0000003a74` |
| `type4-dispatch-record.hex` | `0xfffffc0000073800` | `0x18` | `c50c0e14575038471fddecacf4c6827070936eec6a27fc5cc851e677c986fb23` | Bind pending mask `0x20000` to authenticated target `0xfffffc000002635c` |
| `control-drain-cursors.hex` | `0xfffffc0000003bac` | `0x18` | `fe1021a570e99d6bcda976626c90b111f4de040a2b79f6ffb7fb03d227056e0a` | Publish mirror through `main+0x1a8`, then compare actual through `main+0x1a0` |
| `peer-handles-init.hex` | `0xfffffc0000019184` | `0x20` | `2e3042700bc241bf5ca80601f4f30ab2738c88553876d07c6d5832d0c81c974d` | Store endpoint handles 0x22/0x23 beside the primary firmware-peer lease |
| `mbi-trace-init.hex` | `0xfffffc00000221ac` | `0xc` | `e24ea9af597a04a4a933e1569d0ebb1870bf474b1bb57c01f714b02480bda3f9` | Create group 0's 128-entry MBI trace before scheduler startup |
| `type9-peer-lease.hex` | `0xfffffc0000022de4` | `0x14` | `06b1f93500479874c313189cf3a0f1563149b5b51c122ea9dd0e1181e2808e5b` | Atomically increment the primary peer-send lease on EP21 type 9 |
| `mbi-rx-route.hex` | `0xfffffc000003237c` | `0x64` | `b8b80e44f235ebc946b7d00031bc6d9b1ef3ca1c7ded824521e97d65cfd12ad2` | Append the RX trace before endpoint lookup and callback dispatch |
| `mbi-trace-write.hex` | `0xfffffc00000324f8` | `0x78` | `ff45c6531dac3b7c52080f2046960bc460c4a2307e6dbf3553163b5646c82afe` | Increment the monotonic producer and write the 0x18-byte endpoint/raw/stamp record |

Extraction command:

```sh
dd if=armfw_g17p.im4p of=j700-gfx-a000.macho bs=1 skip=153 count=1596352
```

For these `__text` addresses, file offset is `(link VA - 0xfffffc0000000000) + 0x4000`.

Run `python3 verify.py` in this directory to hash the container and extracted
A000 payload, then compare every admitted instruction window with its exact
Mach-O bytes.
