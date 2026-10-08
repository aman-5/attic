package com.acme

import com.acme.base.{BaseTrait, RunSupport => Runnable}
import scala.collection.mutable._

trait BaseTrait
trait Runnable

class Child extends BaseTrait with Runnable {
  val SIZE = 1
  type Alias = String

  def run(x: Int): Int = helper(x)

  def helper(x: Int): Int = x + SIZE
}

object Hello {
  def greet(name: String): String = helper(name)

  def helper(name: String): String = name
}
