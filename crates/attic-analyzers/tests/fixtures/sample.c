/* Code-like text in comment: struct NotReal { int fake; }; int fake(); */
#include "local.h"
#include <stdio.h>
#define COUNT 3
#define APPLY(x) helper(x)

typedef struct Item {
    int value;
} Item;

enum Mode { Fast, Slow };

static int helper(int x);
int add(int a, int b) {
    return helper(a) + b;
}

static int helper(int x) {
    return x;
}

int global = 1;
