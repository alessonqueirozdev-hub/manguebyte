# manguebyte-wasapi

WASAPI exclusive-mode output and input, for `manguebyte-devices`.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte).
Applications do not use this crate directly: they choose the
`"WASAPI exclusive"` host of a `manguebyte_devices::DeviceManager`, which
uses it. `manguebyte-devices` forbids `unsafe` code, and WASAPI is COM, so
the calls live here, each with the reason it is sound written beside it.

In shared mode every program's sound goes through the Windows audio
engine, which mixes it and runs on its own period, about 10 ms, on top of
the program's buffer. In exclusive mode one program has the device to
itself and moves the device's own format to or from the driver's buffer,
on a period down to the device's shortest. On the development machine's Realtek
laptop output that is 144 frames at 48 kHz, 3 ms, where shared mode
runs on a 10 ms period of its own.

- **Output and input:** `start` plays an output, `start_capture` records
  an input; each calls back once a period. Playing and recording at once is
  two streams, which `manguebyte-devices` joins, following the drift
  between the devices' clocks. There is no loopback capture of an output.
- **Formats:** 32-bit float, 32-bit integer, 24 bits in 32, 16-bit, in
  that order, each tried for real, since drivers do not always do what
  `IsFormatSupported` says; 16-bit in a plain `WAVEFORMATEX` last.
- **Thread:** one per stream, with the "Pro Audio" MMCSS class, which
  makes, uses and releases every COM object of the stream.
- **Refusals:** a device another program holds, one whose "Allow
  applications to take exclusive control" is off, an input Windows'
  privacy settings deny, and drivers that do not do exclusive mode at all
  (Windows' Bluetooth audio among them) are reported as errors; the device
  manager then says why.

## What is tested where

CI, on every system, tests the formats tried and their order, the period
arithmetic, what a failure code means and how a captured packet is read;
on Windows it also compiles the COM calls, checks the codes against the
system's and asks it for a device that does not exist. Streams on real
devices are covered by `tests/hardware.rs`, which no CI job runs on a
device: output has run on a laptop's Realtek output; input and duplex have
not yet run on any.

Those tests open a device only when a variable names it:
`MANGUEBYTE_WASAPI_DEVICE` an output, `MANGUEBYTE_WASAPI_INPUT_DEVICE` an
input. They play silence, and count what an input records without reading
it.

## Licence

MIT OR Apache-2.0, as the rest of the framework.
