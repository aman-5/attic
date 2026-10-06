require "net/http"
require_relative "./support/helper"
load "config/boot.rb"
autoload :WidgetBuilder, "widgets/builder"

module Services
  class Greeter < BaseGreeter
    VERSION = "1.0"
    include Formatters
    extend Helpers
    prepend Hooks

    def self.build(name)
      new(name)
    end

    def initialize(name)
      render(name)
      Helper.call(name)
    end

    def render(name)
      Helper.decorate(name)
    end
  end
end
