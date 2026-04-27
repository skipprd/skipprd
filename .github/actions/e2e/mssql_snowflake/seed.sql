USE testdb;
GO

CREATE TABLE dbo.customers (
    customer_id   NVARCHAR(50) NOT NULL PRIMARY KEY,
    email         NVARCHAR(255),
    first_name    NVARCHAR(100),
    last_name     NVARCHAR(100),
    created_at    DATETIME2
);
GO

INSERT INTO dbo.customers (customer_id, email, first_name, last_name, created_at)
VALUES
  ('c1', 'alice@example.com', 'Alice', 'Smith', '2025-01-01T10:00:00'),
  ('c2', 'bob@example.com',   'Bob',   'Jones', '2025-01-02T11:30:00');
GO

CREATE TABLE dbo.orders (
    order_id      NVARCHAR(50) NOT NULL PRIMARY KEY,
    customer_id   NVARCHAR(50),
    order_status  NVARCHAR(50),
    total_amount  DECIMAL(10,2),
    placed_at     DATETIME2
);
GO

INSERT INTO dbo.orders (order_id, customer_id, order_status, total_amount, placed_at)
VALUES
  ('o1', 'c1', 'PAID', 120.50, '2025-01-03T09:15:00'),
  ('o2', 'c2', 'PAID',  75.00, '2025-01-04T14:45:00');
GO

CREATE TABLE dbo.order_items (
    order_item_id NVARCHAR(50) NOT NULL PRIMARY KEY,
    order_id      NVARCHAR(50),
    product_sku   NVARCHAR(100),
    quantity      INT,
    unit_price    DECIMAL(10,2)
);
GO

INSERT INTO dbo.order_items (order_item_id, order_id, product_sku, quantity, unit_price)
VALUES
  ('oi1', 'o1', 'SKU-RED',   2, 50.00),
  ('oi2', 'o1', 'SKU-BLUE',  1, 20.50),
  ('oi3', 'o2', 'SKU-GREEN', 1, 75.00);
GO
