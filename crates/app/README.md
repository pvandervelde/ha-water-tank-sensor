# Water tank level - The embedded application

Based on: <https://diyodemag.com/projects/arduino_lorawan_enabled_water_tank_level_monitoring_part_1>
and <https://github.com/ajw2060/LoRaWAN-Tank-Sensor-Project>

Measures the water level in a tank and sends it to the service using WIFI.

Uses the following components:

* ESP32-C6 DevKit
* Submerged pressure sensor
* BME280 for temperature and humidity
* Light sensitive diode - To check that the enclosure is still sealed
* Solar panel
* Battery (1.3 Ah)
* ADS1115 - To read the analog values from the pressure sensor
* TSR_1-2450 - To regulate the voltage from the battery to the ESP32. Generating 5V for the devkit
* SDLA12TA - Solar charge module
* EC2-3NU - Relay to turn on/off the sensor
