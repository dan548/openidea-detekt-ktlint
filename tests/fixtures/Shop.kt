package demo

import java.util.*

class Shop(val items: List<Int>) {
    fun Total(): Int {
        val list = items.map {
            it * 42
        }
        return list.sum() + 100
    }
}
