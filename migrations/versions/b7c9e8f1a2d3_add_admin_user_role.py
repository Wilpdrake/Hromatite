"""add admin user role

Revision ID: b7c9e8f1a2d3
Revises: 64a70a6b13d9
Create Date: 2026-05-03 17:16:00.000000

"""
from typing import Sequence, Union

from alembic import op
import sqlalchemy as sa


revision: str = 'b7c9e8f1a2d3'
down_revision: Union[str, Sequence[str], None] = '64a70a6b13d9'
branch_labels: Union[str, Sequence[str], None] = None
depends_on: Union[str, Sequence[str], None] = None


def upgrade() -> None:
    with op.batch_alter_table('admin_users', schema=None) as batch_op:
        batch_op.add_column(sa.Column('role', sa.String(length=20), server_default='user', nullable=False))

    with op.batch_alter_table('admin_users', schema=None) as batch_op:
        batch_op.alter_column('role', server_default=None)


def downgrade() -> None:
    with op.batch_alter_table('admin_users', schema=None) as batch_op:
        batch_op.drop_column('role')
