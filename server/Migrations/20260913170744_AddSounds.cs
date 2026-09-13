using System;
using Microsoft.EntityFrameworkCore.Migrations;
using Npgsql.EntityFrameworkCore.PostgreSQL.Metadata;

#nullable disable

namespace Vorcall.Server.Migrations
{
    /// <inheritdoc />
    public partial class AddSounds : Migration
    {
        /// <inheritdoc />
        protected override void Up(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.CreateTable(
                name: "sounds",
                columns: table => new
                {
                    id = table.Column<long>(type: "bigint", nullable: false)
                        .Annotation("Npgsql:ValueGenerationStrategy", NpgsqlValueGenerationStrategy.IdentityByDefaultColumn),
                    name = table.Column<string>(type: "character varying(32)", maxLength: 32, nullable: false),
                    uploader_id = table.Column<long>(type: "bigint", nullable: true),
                    content_type = table.Column<string>(type: "character varying(64)", maxLength: 64, nullable: false),
                    size = table.Column<long>(type: "bigint", nullable: false),
                    duration_ms = table.Column<int>(type: "integer", nullable: false),
                    complete = table.Column<bool>(type: "boolean", nullable: false),
                    created_at = table.Column<DateTime>(type: "timestamp with time zone", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_sounds", x => x.id);
                    table.ForeignKey(
                        name: "FK_sounds_users_uploader_id",
                        column: x => x.uploader_id,
                        principalTable: "users",
                        principalColumn: "id",
                        onDelete: ReferentialAction.SetNull);
                });

            migrationBuilder.CreateIndex(
                name: "IX_sounds_created_at",
                table: "sounds",
                column: "created_at");

            migrationBuilder.CreateIndex(
                name: "IX_sounds_uploader_id",
                table: "sounds",
                column: "uploader_id");

            // SOUNDPAD (bit 21) joins the everyone defaults, so an existing server's @everyone
            // gains it the way a fresh one gets it from Seed. Spelled out because a migration is a
            // snapshot and must not move when Permissions.Perms.EveryoneDefault does.
            migrationBuilder.Sql("UPDATE roles SET permissions = permissions | 2097152 WHERE is_everyone;");
        }

        /// <inheritdoc />
        protected override void Down(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.Sql("UPDATE roles SET permissions = permissions & ~2097152 WHERE is_everyone;");

            migrationBuilder.DropTable(
                name: "sounds");
        }
    }
}
