#!/usr/bin/env python3
"""Check the weighted join examples used by the advisor presentation.

The row fields follow q3-ivm-baseline/query.sql and the generated intermediate
documented in native-state-read-contracts.md. This is a small algebra checker,
not a simulation of DBSP's storage or scheduling.
"""

from collections import defaultdict
from decimal import Decimal
from itertools import product
from typing import Callable, NamedTuple, TypeVar


class Customer(NamedTuple):
    custkey: int
    segment: str


class Order(NamedTuple):
    orderkey: int
    custkey: int
    date: str
    priority: int


class Lineitem(NamedTuple):
    orderkey: int
    number: int
    price: Decimal
    discount: Decimal
    shipdate: str


class JoinedOrder(NamedTuple):
    orderkey: int
    custkey: int
    segment: str
    date: str
    priority: int


class ViewRow(NamedTuple):
    custkey: int
    segment: str
    orderkey: int
    date: str
    priority: int
    number: int
    price: Decimal
    discount: Decimal
    shipdate: str


T = TypeVar("T")
U = TypeVar("U")
V = TypeVar("V")
Relation = dict[T, int]
BaseState = tuple[Relation[Customer], Relation[Order], Relation[Lineitem]]
JoinBranches = tuple[
    Relation[JoinedOrder],
    Relation[JoinedOrder],
    Relation[ViewRow],
    Relation[ViewRow],
]


def add(*relations: Relation[T]) -> Relation[T]:
    result: dict[T, int] = defaultdict(int)
    for relation in relations:
        for row, weight in relation.items():
            result[row] += weight
    return {row: weight for row, weight in result.items() if weight}


def difference(after: Relation[T], before: Relation[T]) -> Relation[T]:
    return add(after, {row: -weight for row, weight in before.items()})


def join(
    left: Relation[T],
    right: Relation[U],
    matching: Callable[[T, U], bool],
    project: Callable[[T, U], V],
) -> Relation[V]:
    result: dict[V, int] = defaultdict(int)
    for left_row, left_weight in left.items():
        for right_row, right_weight in right.items():
            if matching(left_row, right_row):
                result[project(left_row, right_row)] += left_weight * right_weight
    return {row: weight for row, weight in result.items() if weight}


def orders_customers(
    orders: Relation[Order], customers: Relation[Customer]
) -> Relation[JoinedOrder]:
    return join(
        orders,
        customers,
        lambda order, customer: order.custkey == customer.custkey,
        lambda order, customer: JoinedOrder(
            order.orderkey,
            customer.custkey,
            customer.segment,
            order.date,
            order.priority,
        ),
    )


def lines_joined_orders(
    lines: Relation[Lineitem], joined_orders: Relation[JoinedOrder]
) -> Relation[ViewRow]:
    return join(
        lines,
        joined_orders,
        lambda line, joined_order: line.orderkey == joined_order.orderkey,
        lambda line, joined_order: ViewRow(
            joined_order.custkey,
            joined_order.segment,
            joined_order.orderkey,
            joined_order.date,
            joined_order.priority,
            line.number,
            line.price,
            line.discount,
            line.shipdate,
        ),
    )


def check_update(
    before: BaseState,
    after: BaseState,
) -> JoinBranches:
    customers_before, orders_before, lines_before = before
    customers_after, orders_after, lines_after = after
    delta_customers = difference(customers_after, customers_before)
    delta_orders = difference(orders_after, orders_before)
    delta_lines = difference(lines_after, lines_before)

    # First join: changed Orders see current Customers; changed Customers
    # see Orders from before the transaction.
    first_orders_branch = orders_customers(delta_orders, customers_after)
    first_customers_branch = orders_customers(orders_before, delta_customers)
    delta_joined_orders = add(first_orders_branch, first_customers_branch)
    joined_before = orders_customers(orders_before, customers_before)
    joined_after = orders_customers(orders_after, customers_after)
    assert delta_joined_orders == difference(joined_after, joined_before)

    # Second join has the same asymmetry. This counts a simultaneous
    # Order/Lineitem insertion once, including its cross term.
    first_lines_branch = lines_joined_orders(delta_lines, joined_after)
    second_joined_orders_branch = lines_joined_orders(
        lines_before, delta_joined_orders
    )
    delta_view = add(first_lines_branch, second_joined_orders_branch)
    view_before = lines_joined_orders(lines_before, joined_before)
    view_after = lines_joined_orders(lines_after, joined_after)
    assert delta_view == difference(view_after, view_before)
    assert add(view_before, delta_view) == view_after
    return (
        first_orders_branch,
        first_customers_branch,
        first_lines_branch,
        second_joined_orders_branch,
    )


def main() -> None:
    customer = Customer(7, "BUILDING")
    automobile = Customer(7, "AUTOMOBILE")
    order42 = Order(42, 7, "1995-03-01", 0)
    order42_priority1 = order42._replace(priority=1)
    order43 = Order(43, 7, "1995-03-02", 0)
    order43_priority1 = order43._replace(priority=1)
    line42 = Lineitem(42, 1, Decimal("100.00"), Decimal("0.00"), "1995-03-10")
    line43 = Lineitem(43, 1, Decimal("30.00"), Decimal("0.00"), "1995-03-11")
    line43_price40 = line43._replace(price=Decimal("40.00"))

    baseline = ({customer: 1}, {order42: 1}, {line42: 1})
    inserted = ({customer: 1}, {order42: 1, order43: 1}, {line42: 1, line43: 1})
    branches = check_update(baseline, inserted)
    joined42 = JoinedOrder(42, 7, "BUILDING", "1995-03-01", 0)
    joined43 = JoinedOrder(43, 7, "BUILDING", "1995-03-02", 0)
    view42 = ViewRow(
        7, "BUILDING", 42, "1995-03-01", 0, 1,
        Decimal("100.00"), Decimal("0.00"), "1995-03-10",
    )
    view43 = ViewRow(
        7, "BUILDING", 43, "1995-03-02", 0, 1,
        Decimal("30.00"), Decimal("0.00"), "1995-03-11",
    )
    assert branches == ({joined43: 1}, {}, {view43: 1}, {})
    print("simultaneous Order 43 + Lineitem 43: exact branches and one output checked")

    assert check_update(inserted, baseline) == (
        {joined43: -1}, {}, {}, {view43: -1}
    )
    check_update(inserted, ({automobile: 1}, inserted[1], inserted[2]))
    check_update(
        inserted,
        (inserted[0], {order42: 1, order43_priority1: 1}, inserted[2]),
    )
    check_update(
        inserted,
        (inserted[0], inserted[1], {line42: 1, line43_price40: 1}),
    )
    print("deletion and Customer, Order, Lineitem overwrites checked")

    # Appendix sequence: first replace Customer 7's segment, then remove
    # Order 43 and its Lineitem together. The second transaction must retract
    # the AUTOMOBILE payload retained by the first transaction.
    auto_inserted: BaseState = (
        {automobile: 1}, {order42: 1, order43: 1}, {line42: 1, line43: 1}
    )
    auto_baseline: BaseState = ({automobile: 1}, {order42: 1}, {line42: 1})
    auto_joined42 = joined42._replace(segment="AUTOMOBILE")
    auto_joined43 = joined43._replace(segment="AUTOMOBILE")
    auto_view42 = view42._replace(segment="AUTOMOBILE")
    auto_view43 = view43._replace(segment="AUTOMOBILE")
    assert check_update(inserted, auto_inserted) == (
        {},
        {joined42: -1, auto_joined42: 1, joined43: -1, auto_joined43: 1},
        {},
        {view42: -1, auto_view42: 1, view43: -1, auto_view43: 1},
    )
    assert check_update(auto_inserted, auto_baseline) == (
        {auto_joined43: -1}, {}, {}, {auto_view43: -1}
    )
    print("sequential Customer overwrite and primed Order/Lineitem deletion checked")

    # Slide 2's Order 42 replacement is a signed old/new delta, and the
    # matching Customer and Lineitem yield the corresponding signed view rows.
    priority_state: BaseState = (
        {customer: 1}, {order42_priority1: 1}, {line42: 1}
    )
    assert difference(priority_state[1], baseline[1]) == {
        order42: -1, order42_priority1: 1
    }
    assert check_update(baseline, priority_state) == (
        {joined42: -1, joined42._replace(priority=1): 1},
        {},
        {},
        {view42: -1, view42._replace(priority=1): 1},
    )
    print("Order 42 priority overwrite: -old +new in input and view checked")

    weighted_before = ({customer: 2}, {order42: 3}, {line42: 4})
    weighted_after = ({customer: 1}, {order42: 2}, {line42: 3})
    check_update(weighted_before, weighted_after)
    assert lines_joined_orders(
        weighted_before[2], orders_customers(weighted_before[1], weighted_before[0])
    ) == {view42: 24}
    assert add({view43: 1}, {view43: -1}) == {}
    print("multiplicity 2 × 3 × 4 = 24 and signed cancellation checked")

    # Exhaust all small nonnegative before/after weights for one row in
    # each source. This includes changes on all three inputs at once.
    for weights in product(range(3), repeat=6):
        c0, o0, l0, c1, o1, l1 = weights
        before = ({customer: c0}, {order42: o0}, {line42: l0})
        after = ({customer: c1}, {order42: o1}, {line42: l1})
        check_update(before, after)
    print("729 small weighted before/after states checked against full evaluation")


if __name__ == "__main__":
    main()
