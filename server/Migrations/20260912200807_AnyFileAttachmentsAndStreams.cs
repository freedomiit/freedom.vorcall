using System;
using Microsoft.EntityFrameworkCore.Migrations;
using Npgsql.EntityFrameworkCore.PostgreSQL.Metadata;

#nullable disable

namespace Vorcall.Server.Migrations
{
    /// <inheritdoc />
    public partial class AnyFileAttachmentsAndStreams : Migration
    {
        /// <inheritdoc />
        protected override void Up(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.AlterColumn<string>(
                name: "file_name",
                table: "attachments",
                type: "character varying(255)",
                maxLength: 255,
                nullable: false,
                oldClrType: typeof(string),
                oldType: "character varying(128)",
                oldMaxLength: 128);

            migrationBuilder.AlterColumn<string>(
                name: "content_type",
                table: "attachments",
                type: "character varying(128)",
                maxLength: 128,
                nullable: false,
                oldClrType: typeof(string),
                oldType: "character varying(32)",
                oldMaxLength: 32);

            // The default is the backfill for the rows that already exist: their bytes are on
            // disk, so they are complete. Backfilling false would make the unlinked-upload sweep
            // read every historical attachment as an upload that died in flight. The model
            // declares no default, so EF always writes the column and a new row still starts false.
            migrationBuilder.AddColumn<bool>(
                name: "complete",
                table: "attachments",
                type: "boolean",
                nullable: false,
                defaultValue: true);

            // The default existed only to backfill the rows that predate this column. Dropping it
            // now means a future INSERT that omits `complete` fails loudly instead of quietly
            // claiming the upload finished. EF always writes the column, so nothing in the server
            // depends on it.
            migrationBuilder.Sql("ALTER TABLE attachments ALTER COLUMN complete DROP DEFAULT;");

            migrationBuilder.CreateTable(
                name: "streamed_files",
                columns: table => new
                {
                    id = table.Column<long>(type: "bigint", nullable: false)
                        .Annotation("Npgsql:ValueGenerationStrategy", NpgsqlValueGenerationStrategy.IdentityByDefaultColumn),
                    channel_id = table.Column<long>(type: "bigint", nullable: false),
                    owner_id = table.Column<long>(type: "bigint", nullable: true),
                    message_id = table.Column<long>(type: "bigint", nullable: true),
                    file_name = table.Column<string>(type: "character varying(255)", maxLength: 255, nullable: false),
                    content_type = table.Column<string>(type: "character varying(128)", maxLength: 128, nullable: false),
                    size = table.Column<long>(type: "bigint", nullable: false),
                    created_at = table.Column<DateTime>(type: "timestamp with time zone", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_streamed_files", x => x.id);
                    table.ForeignKey(
                        name: "FK_streamed_files_channels_channel_id",
                        column: x => x.channel_id,
                        principalTable: "channels",
                        principalColumn: "id",
                        onDelete: ReferentialAction.Cascade);
                    table.ForeignKey(
                        name: "FK_streamed_files_messages_message_id",
                        column: x => x.message_id,
                        principalTable: "messages",
                        principalColumn: "id",
                        onDelete: ReferentialAction.SetNull);
                    table.ForeignKey(
                        name: "FK_streamed_files_users_owner_id",
                        column: x => x.owner_id,
                        principalTable: "users",
                        principalColumn: "id",
                        onDelete: ReferentialAction.SetNull);
                });

            migrationBuilder.CreateIndex(
                name: "IX_streamed_files_channel_id",
                table: "streamed_files",
                column: "channel_id");

            migrationBuilder.CreateIndex(
                name: "IX_streamed_files_created_at",
                table: "streamed_files",
                column: "created_at");

            migrationBuilder.CreateIndex(
                name: "IX_streamed_files_message_id",
                table: "streamed_files",
                column: "message_id");

            migrationBuilder.CreateIndex(
                name: "IX_streamed_files_owner_id",
                table: "streamed_files",
                column: "owner_id");
        }

        /// <inheritdoc />
        protected override void Down(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.DropTable(
                name: "streamed_files");

            migrationBuilder.DropColumn(
                name: "complete",
                table: "attachments");

            // Narrowing back is only safe on data the widening never used: this fails if any
            // row has since stored a content_type longer than 32 or a file_name longer than 128.
            migrationBuilder.AlterColumn<string>(
                name: "file_name",
                table: "attachments",
                type: "character varying(128)",
                maxLength: 128,
                nullable: false,
                oldClrType: typeof(string),
                oldType: "character varying(255)",
                oldMaxLength: 255);

            migrationBuilder.AlterColumn<string>(
                name: "content_type",
                table: "attachments",
                type: "character varying(32)",
                maxLength: 32,
                nullable: false,
                oldClrType: typeof(string),
                oldType: "character varying(128)",
                oldMaxLength: 128);
        }
    }
}
