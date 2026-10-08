#!/usr/bin/env python3
"""Check the weighted join examples used by the advisor presentation.

Row fields follow q3-ivm-baseline/query.sql. Both the explanatory
Orders--Lineitem-first and experimental Orders--Customer-first orders are checked.
This is a small algebra checker, not a simulation of DBSP's storage or scheduling.
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


class OrderLine(NamedTuple):
    custkey: int
    orderkey: int
    number: int
    date: str
    priority: int
    price: Decimal
    discount: Decimal
    shipdate: str


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
OriginalBranches = tuple[
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


def check_original_update(
    before: BaseState,
    after: BaseState,
) -> OriginalBranches:
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


def lines_orders(
    lines: Relation[Lineitem], orders: Relation[Order]
) -> Relation[OrderLine]:
    return join(
        lines,
        orders,
        lambda line, order: line.orderkey == order.orderkey,
        lambda line, order: OrderLine(
            order.custkey, order.orderkey, line.number, order.date,
            order.priority, line.price, line.discount, line.shipdate,
        ),
    )


def order_lines_customers(
    order_lines: Relation[OrderLine], customers: Relation[Customer]
) -> Relation[ViewRow]:
    return join(
        order_lines,
        customers,
        lambda row, customer: row.custkey == customer.custkey,
        lambda row, customer: ViewRow(
            customer.custkey, customer.segment, row.orderkey, row.date,
            row.priority, row.number, row.price, row.discount, row.shipdate,
        ),
    )


ExplanatoryBranches = tuple[
    Relation[OrderLine], Relation[OrderLine],
    Relation[ViewRow], Relation[ViewRow],
]


def check_update(before: BaseState, after: BaseState) -> ExplanatoryBranches:
    customers_before, orders_before, lines_before = before
    customers_after, orders_after, lines_after = after
    delta_customers = difference(customers_after, customers_before)
    delta_orders = difference(orders_after, orders_before)
    delta_lines = difference(lines_after, lines_before)

    # Changed lines see after-state Orders; changed Orders see old lines.
    lines_branch = lines_orders(delta_lines, orders_after)
    orders_branch = lines_orders(lines_before, delta_orders)
    delta_order_lines = add(lines_branch, orders_branch)
    order_lines_before = lines_orders(lines_before, orders_before)
    order_lines_after = lines_orders(lines_after, orders_after)
    assert delta_order_lines == difference(order_lines_after, order_lines_before)

    # Changed intermediate rows see after-state Customer. Changed Customer
    # reads retained before-state intermediate rows directly.
    intermediate_branch = order_lines_customers(delta_order_lines, customers_after)
    customers_branch = order_lines_customers(order_lines_before, delta_customers)
    delta_view = add(intermediate_branch, customers_branch)
    view_before = order_lines_customers(order_lines_before, customers_before)
    view_after = order_lines_customers(order_lines_after, customers_after)
    assert delta_view == difference(view_after, view_before)
    assert add(view_before, delta_view) == view_after

    original = check_original_update(before, after)
    assert delta_view == add(original[2], original[3])
    assert view_before == lines_joined_orders(
        lines_before, orders_customers(orders_before, customers_before)
    )
    assert view_after == lines_joined_orders(
        lines_after, orders_customers(orders_after, customers_after)
    )
    return lines_branch, orders_branch, intermediate_branch, customers_branch


def main() -> None:
    customer = Customer(7, "BUILDING")
    automobile = customer._replace(segment="AUTOMOBILE")
    order42 = Order(42, 7, "1995-03-01", 0)
    order43 = Order(43, 7, "1995-03-02", 0)
    line42 = Lineitem(42, 1, Decimal("100.00"), Decimal("0.00"), "1995-03-10")
    line43 = Lineitem(43, 1, Decimal("30.00"), Decimal("0.00"), "1995-03-11")
    ol42 = OrderLine(
        7, 42, 1, "1995-03-01", 0, line42.price, line42.discount, line42.shipdate,
    )
    ol43 = OrderLine(
        7, 43, 1, "1995-03-02", 0, line43.price, line43.discount, line43.shipdate,
    )
    b42 = JoinedOrder(42, 7, "BUILDING", "1995-03-01", 0)
    b43 = JoinedOrder(43, 7, "BUILDING", "1995-03-02", 0)
    v42 = ViewRow(
        7, "BUILDING", 42, "1995-03-01", 0, 1,
        line42.price, line42.discount, line42.shipdate,
    )
    v43 = ViewRow(
        7, "BUILDING", 43, "1995-03-02", 0, 1,
        line43.price, line43.discount, line43.shipdate,
    )
    baseline: BaseState = ({customer: 1}, {order42: 1}, {line42: 1})
    inserted: BaseState = (
        {customer: 1}, {order42: 1, order43: 1}, {line42: 1, line43: 1},
    )

    assert check_update(baseline, inserted) == ({ol43: 1}, {}, {v43: 1}, {})
    assert check_original_update(baseline, inserted) == ({b43: 1}, {}, {v43: 1}, {})
    assert check_update(inserted, baseline) == ({}, {ol43: -1}, {v43: -1}, {})
    assert check_original_update(inserted, baseline) == ({b43: -1}, {}, {}, {v43: -1})
    print("simultaneous insertion/deletion: exact branches in both join orders checked")

    auto_inserted: BaseState = ({automobile: 1}, inserted[1], inserted[2])
    auto_baseline: BaseState = ({automobile: 1}, baseline[1], baseline[2])
    av42 = v42._replace(segment="AUTOMOBILE")
    av43 = v43._replace(segment="AUTOMOBILE")
    ab42 = b42._replace(segment="AUTOMOBILE")
    ab43 = b43._replace(segment="AUTOMOBILE")
    customer_outputs = {v42: -1, av42: 1, v43: -1, av43: 1}
    assert check_update(inserted, auto_inserted) == ({}, {}, {}, customer_outputs)
    assert check_original_update(inserted, auto_inserted) == (
        {}, {b42: -1, ab42: 1, b43: -1, ab43: 1}, {}, customer_outputs,
    )
    assert check_update(auto_inserted, auto_baseline) == (
        {}, {ol43: -1}, {av43: -1}, {},
    )
    assert check_original_update(auto_inserted, auto_baseline) == (
        {ab43: -1}, {}, {}, {av43: -1},
    )
    deleted_customer: BaseState = ({}, inserted[1], inserted[2])
    assert check_update(inserted, deleted_customer) == ({}, {}, {}, {v42: -1, v43: -1})
    assert check_original_update(inserted, deleted_customer) == (
        {}, {b42: -1, b43: -1}, {}, {v42: -1, v43: -1},
    )
    print("Customer overwrite/deletion leaves OL unchanged; retained-row reads checked")

    priority_state: BaseState = (
        baseline[0], {order42._replace(priority=1): 1}, baseline[2],
    )
    ol_priority = {ol42: -1, ol42._replace(priority=1): 1}
    v_priority = {v42: -1, v42._replace(priority=1): 1}
    assert difference(priority_state[1], baseline[1]) == {
        order42: -1, order42._replace(priority=1): 1,
    }
    assert check_update(baseline, priority_state) == ({}, ol_priority, v_priority, {})
    assert check_original_update(baseline, priority_state) == (
        {b42: -1, b42._replace(priority=1): 1}, {}, {}, v_priority,
    )
    price_state: BaseState = (
        inserted[0], inserted[1],
        {line42: 1, line43._replace(price=Decimal("40.00")): 1},
    )
    ol_price = {ol43: -1, ol43._replace(price=Decimal("40.00")): 1}
    v_price = {v43: -1, v43._replace(price=Decimal("40.00")): 1}
    assert check_update(inserted, price_state) == (ol_price, {}, v_price, {})
    assert check_original_update(inserted, price_state) == ({}, {}, v_price, {})
    print("Order/Lineitem overwrites: exact intermediate/output branches checked")

    weighted_before: BaseState = ({customer: 2}, {order42: 3}, {line42: 4})
    weighted_after: BaseState = ({customer: 1}, {order42: 2}, {line42: 3})
    assert check_update(weighted_before, weighted_after) == (
        {ol42: -2}, {ol42: -4}, {v42: -6}, {v42: -12},
    )
    assert check_original_update(weighted_before, weighted_after) == (
        {b42: -1}, {b42: -3}, {v42: -2}, {v42: -16},
    )
    weighted_lines = lines_orders(weighted_before[2], weighted_before[1])
    assert order_lines_customers(weighted_lines, weighted_before[0]) == {v42: 24}
    assert add({ol43: 1}, {ol43: -1}) == {}
    assert add({v43: 1}, {v43: -1}) == {}
    assert check_update(inserted, inserted) == ({}, {}, {}, {})
    assert check_original_update(inserted, inserted) == ({}, {}, {}, {})
    print("multiplicity, cancellation, unchanged transactions: exact branches checked")

    # Keep the original exhaustive nonnegative cases and also check signed
    # states, verifying both groupings against full evaluation for every case.
    for domain in (range(3), range(-1, 2)):
        for weights in product(domain, repeat=6):
            c0, o0, l0, c1, o1, l1 = weights
            before = ({customer: c0}, {order42: o0}, {line42: l0})
            after = ({customer: c1}, {order42: o1}, {line42: l1})
            check_update(before, after)
    print("729 nonnegative and 729 signed cases: both programs equal full evaluation")


if __name__ == "__main__":
    main()
