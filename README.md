# ha-water-tank-sensor

Stores the code and schematics for the system we use to monitor the level in the water tanks on our
property.

Consists of a IoT device that measures the water level in the tank and sends it to the service
using WIFI. The service extracts the data from the message and sends it to a Grafana stack.

* Schema -> KiCad
* PCB -> KiCad (not done)

## The embedded device

### Hardware

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
* XL6009E1 - DC-DC converter to generate 24V for the pressure sensor
* BS170 - N-channel MOSFET to turn on/off the relay

### Pin assignments

* ESP32-C6 DevKit
  * 5V - TSR_1-2450
  * 3V3 - BME280, ADS1115, Light sensitive diode
  * GND - BME280, ADS1115, Light sensitive diode
  * SDA - BME280, ADS1115,
  * SCL - BME280, ADS1115,
  * GPIO 10 - SDA (ADS1115, BME280)
  * GPIO 11 - SCL (ADS1115, BME280)
  * GPIO 18 - Boost enable (BS170)
* ADS1115
  * GND - ESP32
  * VDD - ESP32
  * SDA - ESP32
  * SCL - ESP32
  * ALERT - Not connected
  * A0 - Light sensitive diode
  * A1 - Pressure sensor
  * A2 - Not connected
  * A3 - Battery sensor
* BME280

### Software

## The service

## Communication

* Message format
* connectivity
