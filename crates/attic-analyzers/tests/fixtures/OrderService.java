package com.example.orders;

import java.util.List;
import java.util.Map;
import com.example.shared.Identifiable;
import static java.util.Collections.unmodifiableList;

/**
 * Order service fixture. Code-like text inside comments/strings:
 * public class NotReal { void fake() {} }
 */
public class OrderService extends BaseService implements Identifiable, Comparable<OrderService> {

    private static final int MAX_RETRIES = 3;
    private List<String> tags;

    public OrderService(List<String> tags) {
        this.tags = unmodifiableList(tags);
    }

    @Override
    public String id() {
        return "order-service";
    }

    protected int compute(int base, int factor) {
        return base * factor + MAX_RETRIES;
    }

    // Overload #1
    int compute(int base) {
        return compute(base, 1);
    }

    // Overload #2 — same name, different params (disambiguation target).
    int compute(String raw) {
        return raw.length();
    }
}
