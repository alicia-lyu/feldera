-- Base circuit: the three Q3 tables with primary keys and no view. The driver applies each
-- transaction here first, so the primary-key input maps hold the base relations and their
-- net delta is what the view circuit (q3.sql) receives. Compile with --alltables, because no
-- view reads the tables.
CREATE TABLE Customer (
    c_custkey BIGINT NOT NULL PRIMARY KEY,
    c_mktsegment VARCHAR NOT NULL
);

CREATE TABLE Orders (
    o_orderkey BIGINT NOT NULL PRIMARY KEY,
    o_custkey BIGINT NOT NULL,
    o_orderdate DATE NOT NULL,
    o_shippriority INTEGER NOT NULL
);

CREATE TABLE Lineitem (
    l_orderkey BIGINT NOT NULL,
    l_linenumber INTEGER NOT NULL,
    l_extendedprice DECIMAL(15, 2) NOT NULL,
    l_discount DECIMAL(4, 2) NOT NULL,
    l_shipdate DATE NOT NULL,
    PRIMARY KEY (l_orderkey, l_linenumber)
);
