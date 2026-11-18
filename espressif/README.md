# Espressif SoC models from esp32sim

`esp32s31/`, `esp-soc/`, `esp-periph/` and `emu-core/` (package `esp32sim-emu-core`) are imported
from [esp32sim](https://github.com/mikroverk/esp32sim) at commit
`d043ff1dd3e53aca18a23f4f2b18bfb038a57667` (MIT, see [LICENSE](LICENSE)). They model the
ESP32-S31 for the ESP32-S31-Function-CoreBoard-1 Linux board ([docs/esp32s31.md](../docs/esp32s31.md)).

Left out: esp32sim's committed firmware images (`esp32s31/firmware/`) and the golden tests that
boot them; `tests/sd_image.rs` is the one shared test helper `esp-soc` needs. Changes made here
for Linux are listed in `docs/esp32s31.md`.
