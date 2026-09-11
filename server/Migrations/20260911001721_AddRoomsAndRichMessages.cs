using System;
using Microsoft.EntityFrameworkCore.Migrations;
using Npgsql.EntityFrameworkCore.PostgreSQL.Metadata;

#nullable disable

namespace Vorcall.Server.Migrations
{
    /// <inheritdoc />
    public partial class AddRoomsAndRichMessages : Migration
    {
        /// <inheritdoc />
        protected override void Up(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.AddColumn<DateTime>(
                name: "deleted_at",
                table: "messages",
                type: "timestamp with time zone",
                nullable: true);

            migrationBuilder.AddColumn<DateTime>(
                name: "edited_at",
                table: "messages",
                type: "timestamp with time zone",
                nullable: true);

            migrationBuilder.AddColumn<long[]>(
                name: "mention_ids",
                table: "messages",
                type: "bigint[]",
                nullable: false,
                defaultValueSql: "ARRAY[]::bigint[]");

            migrationBuilder.AddColumn<long>(
                name: "reply_to_id",
                table: "messages",
                type: "bigint",
                nullable: true);

            migrationBuilder.CreateTable(
                name: "attachments",
                columns: table => new
                {
                    id = table.Column<long>(type: "bigint", nullable: false)
                        .Annotation("Npgsql:ValueGenerationStrategy", NpgsqlValueGenerationStrategy.IdentityByDefaultColumn),
                    room_id = table.Column<string>(type: "character varying(32)", maxLength: 32, nullable: false),
                    uploader_id = table.Column<long>(type: "bigint", nullable: true),
                    message_id = table.Column<long>(type: "bigint", nullable: true),
                    file_name = table.Column<string>(type: "character varying(128)", maxLength: 128, nullable: false),
                    content_type = table.Column<string>(type: "character varying(32)", maxLength: 32, nullable: false),
                    size = table.Column<long>(type: "bigint", nullable: false),
                    created_at = table.Column<DateTime>(type: "timestamp with time zone", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_attachments", x => x.id);
                    table.ForeignKey(
                        name: "FK_attachments_messages_message_id",
                        column: x => x.message_id,
                        principalTable: "messages",
                        principalColumn: "id",
                        onDelete: ReferentialAction.SetNull);
                    table.ForeignKey(
                        name: "FK_attachments_users_uploader_id",
                        column: x => x.uploader_id,
                        principalTable: "users",
                        principalColumn: "id",
                        onDelete: ReferentialAction.SetNull);
                });

            migrationBuilder.CreateTable(
                name: "reactions",
                columns: table => new
                {
                    message_id = table.Column<long>(type: "bigint", nullable: false),
                    user_id = table.Column<long>(type: "bigint", nullable: false),
                    emoji = table.Column<string>(type: "character varying(16)", maxLength: 16, nullable: false),
                    created_at = table.Column<DateTime>(type: "timestamp with time zone", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_reactions", x => new { x.message_id, x.user_id, x.emoji });
                    table.ForeignKey(
                        name: "FK_reactions_messages_message_id",
                        column: x => x.message_id,
                        principalTable: "messages",
                        principalColumn: "id",
                        onDelete: ReferentialAction.Cascade);
                    table.ForeignKey(
                        name: "FK_reactions_users_user_id",
                        column: x => x.user_id,
                        principalTable: "users",
                        principalColumn: "id",
                        onDelete: ReferentialAction.Cascade);
                });

            migrationBuilder.CreateTable(
                name: "rooms",
                columns: table => new
                {
                    id = table.Column<string>(type: "character varying(32)", maxLength: 32, nullable: false),
                    kind = table.Column<short>(type: "smallint", nullable: false),
                    name = table.Column<string>(type: "character varying(32)", maxLength: 32, nullable: false),
                    created_by = table.Column<long>(type: "bigint", nullable: true),
                    created_at = table.Column<DateTime>(type: "timestamp with time zone", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_rooms", x => x.id);
                    table.ForeignKey(
                        name: "FK_rooms_users_created_by",
                        column: x => x.created_by,
                        principalTable: "users",
                        principalColumn: "id",
                        onDelete: ReferentialAction.SetNull);
                });

            migrationBuilder.CreateTable(
                name: "room_members",
                columns: table => new
                {
                    room_id = table.Column<string>(type: "character varying(32)", maxLength: 32, nullable: false),
                    user_id = table.Column<long>(type: "bigint", nullable: false),
                    joined_at = table.Column<DateTime>(type: "timestamp with time zone", nullable: false),
                    last_read_message_id = table.Column<long>(type: "bigint", nullable: false, defaultValue: 0L)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_room_members", x => new { x.room_id, x.user_id });
                    table.ForeignKey(
                        name: "FK_room_members_rooms_room_id",
                        column: x => x.room_id,
                        principalTable: "rooms",
                        principalColumn: "id",
                        onDelete: ReferentialAction.Cascade);
                    table.ForeignKey(
                        name: "FK_room_members_users_user_id",
                        column: x => x.user_id,
                        principalTable: "users",
                        principalColumn: "id",
                        onDelete: ReferentialAction.Cascade);
                });

            migrationBuilder.CreateIndex(
                name: "IX_attachments_created_at",
                table: "attachments",
                column: "created_at");

            migrationBuilder.CreateIndex(
                name: "IX_attachments_message_id",
                table: "attachments",
                column: "message_id");

            migrationBuilder.CreateIndex(
                name: "IX_attachments_uploader_id",
                table: "attachments",
                column: "uploader_id");

            migrationBuilder.CreateIndex(
                name: "IX_reactions_user_id",
                table: "reactions",
                column: "user_id");

            migrationBuilder.CreateIndex(
                name: "IX_room_members_user_id",
                table: "room_members",
                column: "user_id");

            migrationBuilder.CreateIndex(
                name: "IX_rooms_created_by",
                table: "rooms",
                column: "created_by");

            // general is the room every account belongs to and nobody may leave, so it exists as
            // a row from this migration on rather than being created by anyone. Every account
            // that already exists is backfilled into it with its cursor at the newest message:
            // nobody is owed an unread badge for the history they have been reading all along.
            migrationBuilder.Sql(
                """
                INSERT INTO rooms (id, kind, name, created_by, created_at)
                VALUES ('general', 1, 'general', NULL, now())
                ON CONFLICT (id) DO NOTHING;

                INSERT INTO room_members (room_id, user_id, joined_at, last_read_message_id)
                SELECT 'general', u.id, now(), COALESCE((SELECT MAX(m.id) FROM messages m WHERE m.room_id = 'general'), 0)
                FROM users u
                ON CONFLICT DO NOTHING;
                """);
        }

        /// <inheritdoc />
        protected override void Down(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.DropTable(
                name: "attachments");

            migrationBuilder.DropTable(
                name: "reactions");

            migrationBuilder.DropTable(
                name: "room_members");

            migrationBuilder.DropTable(
                name: "rooms");

            migrationBuilder.DropColumn(
                name: "deleted_at",
                table: "messages");

            migrationBuilder.DropColumn(
                name: "edited_at",
                table: "messages");

            migrationBuilder.DropColumn(
                name: "mention_ids",
                table: "messages");

            migrationBuilder.DropColumn(
                name: "reply_to_id",
                table: "messages");
        }
    }
}
