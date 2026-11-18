#include <Arduino.h>
#include <Mouse.h>
#include <avr/pgmspace.h>

#include "scroll_profile.h"

constexpr uint8_t BUTTON_PIN = 2;
constexpr uint32_t DEBOUNCE_MS = 20;

bool scrolling = false;
bool randomSeeded = false;
uint8_t rawButtonState = HIGH;
uint8_t stableButtonState = HIGH;
uint32_t buttonChangedAtMs = 0;

int8_t scrollDirection = -1;
uint8_t stepsRemaining = 0;
uint32_t lastStepAtUs = 0;
uint32_t waitBeforeStepUs = 0;

uint32_t chooseDelay(const uint32_t* values, uint16_t count) {
    const uint16_t index = static_cast<uint16_t>(random(count));
    return pgm_read_dword(values + index);
}

uint8_t chooseRunLength(int8_t direction) {
    if (direction > 0) {
        const uint16_t count = sizeof(UP_RUN_LENGTHS) / sizeof(UP_RUN_LENGTHS[0]);
        return pgm_read_byte(UP_RUN_LENGTHS + random(count));
    }

    const uint16_t count = sizeof(DOWN_RUN_LENGTHS) / sizeof(DOWN_RUN_LENGTHS[0]);
    return pgm_read_byte(DOWN_RUN_LENGTHS + random(count));
}

uint32_t chooseStepDelay(int8_t direction) {
    if (direction > 0) {
        return chooseDelay(UP_INTERVALS_US, sizeof(UP_INTERVALS_US) / sizeof(UP_INTERVALS_US[0]));
    }
    return chooseDelay(DOWN_INTERVALS_US, sizeof(DOWN_INTERVALS_US) / sizeof(DOWN_INTERVALS_US[0]));
}

uint32_t chooseReversalDelay(int8_t nextDirection) {
    if (nextDirection > 0) {
        return chooseDelay(TO_UP_INTERVALS_US, sizeof(TO_UP_INTERVALS_US) / sizeof(TO_UP_INTERVALS_US[0]));
    }
    return chooseDelay(TO_DOWN_INTERVALS_US, sizeof(TO_DOWN_INTERVALS_US) / sizeof(TO_DOWN_INTERVALS_US[0]));
}

void toggleScrolling() {
    scrolling = !scrolling;
    if (!scrolling) {
        return;
    }

    // First press timing supplies a different pseudorandom seed across boots.
    // Do not restart the random sequence on subsequent start/stop presses.
    if (!randomSeeded) {
        const uint32_t seed = micros();
        randomSeed(seed == 0 ? 1 : seed);
        randomSeeded = true;
    }

    scrollDirection = random(2) == 0 ? -1 : 1;
    stepsRemaining = chooseRunLength(scrollDirection);
    lastStepAtUs = micros();
    waitBeforeStepUs = 0;  // First wheel step follows the debounced press.
}

void updateButton() {
    const uint32_t nowMs = millis();
    const uint8_t state = digitalRead(BUTTON_PIN);

    if (state != rawButtonState) {
        rawButtonState = state;
        buttonChangedAtMs = nowMs;
    }

    // Both press and release must be stable. Holding the button never repeats.
    if (rawButtonState != stableButtonState &&
        static_cast<uint32_t>(nowMs - buttonChangedAtMs) >= DEBOUNCE_MS) {
        stableButtonState = rawButtonState;
        if (stableButtonState == LOW) {
            toggleScrolling();
        }
    }
}

void updateScroll() {
    if (!scrolling) {
        return;
    }

    const uint32_t nowUs = micros();
    // Unsigned subtraction remains valid when micros() wraps around.
    if (static_cast<uint32_t>(nowUs - lastStepAtUs) < waitBeforeStepUs) {
        return;
    }

    lastStepAtUs = nowUs;
    Mouse.move(0, 0, scrollDirection);
    --stepsRemaining;

    if (stepsRemaining == 0) {
        scrollDirection = -scrollDirection;
        stepsRemaining = chooseRunLength(scrollDirection);
        waitBeforeStepUs = chooseReversalDelay(scrollDirection);
    } else {
        waitBeforeStepUs = chooseStepDelay(scrollDirection);
    }
    // Send at most one report per loop. Never catch up with a burst of reports
    // after a USB delay; give button handling another turn before the next step.
}

void setup() {
    pinMode(BUTTON_PIN, INPUT_PULLUP);
    rawButtonState = digitalRead(BUTTON_PIN);
    stableButtonState = rawButtonState;
    buttonChangedAtMs = millis();
    Mouse.begin();
    // Playback stays off at startup, including when the button is held.
}

void loop() {
    updateButton();
    updateScroll();
}
