package com.acme.orders

import com.acme.base.BaseController
import com.acme.base.RunnableSupport as Runnable
import com.acme.shared.SupportService
import org.springframework.web.bind.annotation.GetMapping
import org.springframework.web.bind.annotation.RestController

@RestController
class OrderController(
    private val service: OrderService,
    val support: SupportService
) : BaseController(), Runnable, SupportService by support {
    @GetMapping("/orders/{id}")
    fun find(id: Long): Order = service.find(id)

    companion object Paths {
        const val BASE = "/orders"
    }

    val enabled = true
}

interface OrderService {
    fun find(id: Long): Order
}

object OrderMetrics

typealias OrderId = Long

fun Order.total(): Long = lines.sumOf { it.price }
