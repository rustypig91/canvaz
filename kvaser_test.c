#include "canlib.h"
// #include "kvrlib.h"
#include <stdio.h>

int main() {
    int n = 0;
    // kvrInitializeLibrary();
    canInitializeLibrary();
    canStatus status = canGetNumberOfChannels(&n);
    if (status != canOK) {
        printf("Failed to get number of CAN channels: %d\n", status);
        return 1;
    }
    printf("Number of CAN channels: %d\n", n);
    return 0;
}