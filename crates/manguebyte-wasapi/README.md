# manguebyte-wasapi

WASAPI exclusive-mode output, for `manguebyte-devices`.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte).
Applications do not use this crate directly: they choose the
`"WASAPI exclusive"` host of a `manguebyte_devices::DeviceManager`, which
uses it. `manguebyte-devices` forbids `unsafe` code, and WASAPI is COM, so
the calls live here, each with the reason it is sound written beside it.

In shared mode every program's sound goes through the Windows audio
engine, which mixes it and runs on its own period, about 10 ms, on top of
the program's buffer. In exclusive mode one program has the device to
itself and writes the device's own format into the driver's buffer, on a
period down to the device's shortest. On the development machine's Realtek
laptop output that is 144 frames at 48 kHz, 3 ms, where shared mode
runs on a 10 ms period of its own.

- **Output only.** Exclusive capture is not here.
- **Formats:** 32-bit float, 32-bit integer, 24 bits in 32, 16-bit, in
  that order, each tried for real, since drivers do not always do what
  `IsFormatSupported` says; 16-bit in a plain `WAVEFORMATEX` last.
- **Thread:** one per stream, with the "Pro Audio" MMCSS class, which
  makes, uses and releases every COM object of the stream.
- **Refusals:** a device another program holds, one whose "Allow
  applications to take exclusive control" is off, and drivers that do not
  do exclusive mode at all (Windows' Bluetooth audio among them) are
  reported as errors; the device manager then says why.

Its hardware tests open a device only when `MANGUEBYTE_WASAPI_DEVICE`
names one, and play silence.

## Licence

MIT OR Apache-2.0, as the rest of the framework.
