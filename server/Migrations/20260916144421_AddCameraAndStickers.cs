using System;
using Microsoft.EntityFrameworkCore.Migrations;
using Npgsql.EntityFrameworkCore.PostgreSQL.Metadata;

#nullable disable

namespace Vorcall.Server.Migrations
{
    /// <inheritdoc />
    public partial class AddCameraAndStickers : Migration
    {
        /// <inheritdoc />
        protected override void Up(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.AddColumn<bool>(
                name: "sticker",
                table: "messages",
                type: "boolean",
                nullable: false,
                defaultValue: false);

            migrationBuilder.AddColumn<long>(
                name: "sticker_id",
                table: "messages",
                type: "bigint",
                nullable: true);

            migrationBuilder.CreateTable(
                name: "stickers",
                columns: table => new
                {
                    id = table.Column<long>(type: "bigint", nullable: false)
                        .Annotation("Npgsql:ValueGenerationStrategy", NpgsqlValueGenerationStrategy.IdentityByDefaultColumn),
                    name = table.Column<string>(type: "character varying(32)", maxLength: 32, nullable: false),
                    uploader_id = table.Column<long>(type: "bigint", nullable: true),
                    content_type = table.Column<string>(type: "character varying(32)", maxLength: 32, nullable: false),
                    size = table.Column<long>(type: "bigint", nullable: false),
                    complete = table.Column<bool>(type: "boolean", nullable: false),
                    created_at = table.Column<DateTime>(type: "timestamp with time zone", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_stickers", x => x.id);
                    table.ForeignKey(
                        name: "FK_stickers_users_uploader_id",
                        column: x => x.uploader_id,
                        principalTable: "users",
                        principalColumn: "id",
                        onDelete: ReferentialAction.SetNull);
                });

            migrationBuilder.CreateIndex(
                name: "IX_messages_sticker_id",
                table: "messages",
                column: "sticker_id");

            migrationBuilder.CreateIndex(
                name: "IX_stickers_created_at",
                table: "stickers",
                column: "created_at");

            migrationBuilder.CreateIndex(
                name: "IX_stickers_uploader_id",
                table: "stickers",
                column: "uploader_id");

            migrationBuilder.AddForeignKey(
                name: "FK_messages_stickers_sticker_id",
                table: "messages",
                column: "sticker_id",
                principalTable: "stickers",
                principalColumn: "id",
                onDelete: ReferentialAction.SetNull);

            // VIDEO (bit 23) joins the everyone defaults, so an existing server's @everyone gains it
            // the way a fresh one gets it from Seed. Spelled out because a migration is a snapshot and
            // must not move when Permissions.Perms.EveryoneDefault does.
            migrationBuilder.Sql("UPDATE roles SET permissions = permissions | 8388608 WHERE is_everyone;");
        }

        /// <inheritdoc />
        protected override void Down(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.Sql("UPDATE roles SET permissions = permissions & ~8388608 WHERE is_everyone;");

            migrationBuilder.DropForeignKey(
                name: "FK_messages_stickers_sticker_id",
                table: "messages");

            migrationBuilder.DropTable(
                name: "stickers");

            migrationBuilder.DropIndex(
                name: "IX_messages_sticker_id",
                table: "messages");

            migrationBuilder.DropColumn(
                name: "sticker",
                table: "messages");

            migrationBuilder.DropColumn(
                name: "sticker_id",
                table: "messages");
        }
    }
}
