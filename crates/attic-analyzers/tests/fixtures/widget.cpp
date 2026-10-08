/* Code-like text in comment: class NotReal { void fake(); }; */
#include "widget.hpp"
#include <vector>
#define COUNT 3

namespace app::core {

template <typename T>
class Base {};

class Widget : public Base<int>, private Detail {
public:
    Widget();
    ~Widget();
    void run();
    int operator()(int x) const { return helper(x); }
};

using namespace support;
using util::Helper;

template <typename T>
T helper(T value) {
    return value;
}

Widget::Widget() {}
Widget::~Widget() {}
void Widget::run() {
    helper(1);
}

} // namespace app::core
