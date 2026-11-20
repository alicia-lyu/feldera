-- Base circuit: the three MI_P tables with primary keys and no view. The driver applies each
-- transaction here first, so the primary-key input maps hold the base relations and their
-- net delta is what the view circuit (q14.sql) and the merged index receive. Compile with
-- --alltables, because no view reads the tables. The columns are those of
-- ../merged-index-p/schema.sql.
CREATE TABLE Part (
    p_partkey BIGINT NOT NULL PRIMARY KEY,
    p_type VARCHAR NOT NULL
);

CREATE TABLE Partsupp (
    ps_partkey BIGINT NOT NULL,
    ps_suppkey BIGINT NOT NULL,
    ps_availqty INTEGER NOT NULL,
    PRIMARY KEY (ps_partkey, ps_suppkey)
);

CREATE TABLE Lineitem (
    l_orderkey BIGINT NOT NULL,
    l_linenumber INTEGER NOT NULL,
    l_partkey BIGINT NOT NULL,
    l_suppkey BIGINT NOT NULL,
    l_extendedprice DECIMAL(15, 2) NOT NULL,
    l_discount DECIMAL(4, 2) NOT NULL,
    l_shipdate DATE NOT NULL,
    PRIMARY KEY (l_orderkey, l_linenumber)
);
