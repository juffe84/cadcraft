# Terrain work items for issue #3361

The first increments use ordinary drawing entities. A TIN is a named group of
triangular 3DFACEs in model space. Work can be reviewed and tested independently.

| Work item | Status | Acceptance check |
| --- | --- | --- |
| Build terrain from points and breaklines | Implemented | Select CAD POINTs, lines and 3D polylines or pass XYZ paths and outer or hole boundaries to `TIN`. The output preserves sample heights and constrained edges. |
| Generate terrain contours | Implemented | `TIN.CONTOURS` writes joined 3D polylines and can label index contours. Verify a contour crossing two triangles remains continuous. |
| Compare existing and design terrain | Implemented | `TIN.VOLUMES` compares two named groups and reports cut, fill, net fill and overlapping area in drawing units. |
| Persist the triangle surface | Implemented as standard CAD entities | Save as DWG or DXF and reopen with the grouped faces and their coordinates intact. |
| Edit an existing terrain mesh | Implemented | Change a shared point elevation or position, erase a set of triangles, and cut to a closed boundary or remove a hole while preserving sampled heights. |
| Surface editor and local topology | Implemented | Select the target surface in a modeless editor, swap a shared edge, or apply a 3D breakline and check that the line defines a continuous graded edge. |
| Rebuild linked terrain from source objects | Implemented | Build a linked TIN from drawing points and breaklines, edit a source, and verify the updated mesh and conflict flags after saving and reopening. |
| Link separate outer boundaries and holes | Pending | Apply associative boundary and hole definitions alongside the source objects. |
| Display terrain in 3D | Implemented | Switch between drawing colors, elevation bands and slope bands. Adjust surface transparency and triangle edges. |
| Add terrain analysis and display options | Pending | Provide slope arrows, watershed analysis and independent display of contours and points. |
| LandXML surface import | Implemented | Open a LandXML or Inframodel file and verify the named TIN groups, original XYZ coordinates and drawing units. |
| LandXML surface export | Implemented | Export the current TIN faces and linked source geometry as LandXML 1.2. |
| Survey-specific inputs and native surface metadata | Pending | Read survey point files and COGO objects directly and preserve surface definitions and edit history separately from baked faces. |

## Try the implemented commands

Create a named surface from the command line with JSON.

```text
TIN {"name":"Existing","points":[[0,0,0],[10,0,2],[10,10,2],[0,10,0]]}
TIN.CONTOURS {"group":"Existing","interval":0.5,"majorEvery":5,"labelHeight":0.4}
TIN {"name":"Design","points":[[0,0,1],[10,0,3],[10,10,3],[0,10,1]]}
TIN.VOLUMES {"existing":"Existing","design":"Design"}
```

In the GUI, select the model-space source objects and run **Draw → Terrain →
Build Terrain Surface**. **Draw Terrain Contours** asks for the interval and defaults to one drawing
unit between levels. The JSON form also lets you configure index labels.

Each XY location needs one elevation. Split crossing constraints at a shared
XYZ vertex. Cut and fill are computed over the common XY footprint of two
surfaces. Areas outside that footprint contribute no volume.

Open **Draw → Terrain → Measure Terrain Earthwork** and choose the existing
and design surfaces from the two menus. The picker includes unnamed groups
such as `*A1` and lists only groups of triangular model-space faces.

Open a LandXML TIN through **File → Open** using the **LandXML terrain (XML, LANDXML)** filter.
The imported surfaces are named groups available to the terrain tools. Use **Save As** to keep the drawing as DWG or DXF.

In **Surface Editor**, select a surface and choose **Export LandXML 1.2…**.
Choose the drawing units or label the output coordinates as metres, millimetres
or feet. This choice leaves the coordinate values as entered in the drawing.
The file includes the current triangles, linked source lines as breaklines
and isolated source heights as data points. Curves use the same chord sampling
as linked terrain triangulation. The LandXML reader opens the explicit faces.
**File → Export** or `EXPORT {"path":"terrain.xml"}` exports all terrain surfaces.
Use `group` for one surface and `unit` to override the drawing unit code.
Unitless drawings use metres for metric drawings and feet for imperial drawings.

Choose **Survey Compatibility (Metres)** in the LandXML export menu for a survey
importer that rejects intersecting edges or very narrow triangles. This uses a
1 mm grid in the export, rebuilds the current terrain on the grid, and splits
the source lines where they cross. Coincident sections with identical elevations
are written once. Different elevations remain available on their source lines.
The current terrain coverage is used to retain existing cutouts, with small
adjustments at the grid resolution. Use this option when the drawing coordinates
are already metres. The command form is
`EXPORT {"path":"terrain.xml","group":"Site","unit":6,"surveyCompatibility":true}`.

Open **View → Terrain Display...** to choose elevation bands, slope bands or the drawing's colors. Changing a setting from the plan opens the terrain in 3D Orbit from above.
The legend shows the elevation range in drawing units or slopes from 0° to 90°.
Band count, transparency and triangle edges are configurable for the current drawing's orbit view.


Use **Draw → Terrain → Edit Surface** to edit an existing TIN. When more than one
surface is open in the same drawing, pick a face to choose its surface or use the
**Name** prompt. Use Endpoint snap to choose a vertex and enter its new elevation
or position. An XY move leaves the vertex at the same height. A move that folds
or overlaps the mesh is refused. **Remove Surface Triangles** selects individual
triangles and applies the selection on Enter.

For clipping or cutting a hole, choose a closed straight-segment 2D or 3D
polyline. The plan outline cuts crossing faces and takes the new vertex heights
from the existing TIN. The boundary polyline stays in the drawing. The resulting
faces are a mesh edit and can be reverted with Undo.


Open **Draw → Terrain → Surface Editor...** to choose a named surface once and
apply the individual tools while keeping the panel visible. **Swap Triangle Edge**
uses two adjacent triangles from the selected surface and preserves their outline
and four elevations. **Apply 3D Breakline** accepts an existing line or 3D
polyline wholly on the surface and uses its XYZ to split crossed terrain edges
and shape the adjoining faces. Both commands are undoable. The saved drawing
stores the resulting TIN faces and the original line entity.

**Draw → Terrain → Build Terrain Surface** and **Build Linked Terrain** build a surface whose selected model-space
points, lines, arcs and 2D or 3D polylines remain its sources. The mesh and red flags
update at the end of a source edit and are included in the same Undo step. The
**Properties Inspector** exposes **Start Z** and **End Z** for a line, **Center Z**
for an arc, **Elevation** for a 2D polyline and **Height Offset** for a 3D polyline.

Arcs and curved 2D polyline segments are sampled automatically at a chord tolerance of 0.01 drawing units. The original curves remain linked, so curve edits resample them. Supply coordinates directly or pass `standalone: true` to `TIN` to build detached faces.

Crossings and coincident endpoints are joined in XY. If the candidate elevations
differ by more than 0.0001 drawing units, the higher elevation is used at that
node and an upright red flag with filled faces is placed on **TIN-CONFLICTS**.
The flag's base is the conflict point. Its folded shape has visible area in
plan and orbit and keeps its fill in orbit wireframe. Older generated wire
flags are upgraded when opened.
The generated terrain faces are on
**TIN-SURFACES**, which can be turned off to select the source line beneath a face.
The source associations are stored using ordinary drawing groups.

On a linked surface, **Source Elevations** in the drawing area labels every
visible source position in the active surface. Identical XYZ positions share
one label. Different heights at the same XY have separate labels. Set the text
size with **Elevation Label Size**. The labels stay on the drawing when the
**Surface Editor** is closed. The editor shows the conflict count and lets you
select a surface when the drawing has several.

Source lines appear pale blue. Click a segment to highlight the whole line in
green and the active segment in red. The previous and next arrows move through
the line's segments. Click a vertex, isolated point or elevation label to
focus its Z field with the old value selected. Type the new elevation and
press Enter. For coincident picks, **Points at This Location** lists the source
points so you can choose which one to edit.

Choose **Set Chain Elevation…** to assign one Z elevation to every vertex of the
selected source chain. **Copy Chain…** creates a parallel chain at a distance
or runs a slope to a height or another surface. The slope uses vertical to
horizontal units. A slope of `1:2` with an elevation difference of one metre
runs two metres sideways. For a target surface, choose **Down** or **Up**.
The tool follows the first intersection and rejects a grade that runs out of
target triangles. **Round Outside Corners** samples the rounded joins in the
new 3D polyline. Choose **Preview Copy** to check the side and shape in plan or
orbit, then **Create Copy**. **Add Copy to Source Surface** also rebuilds the
source terrain with the new chain.
**Pick Position** fills X and Y from a click at the vertex elevation.
**Pick Elevation from Line** samples the clicked location along another visible
line, polyline or arc. Use **Apply** to commit the coordinates.

**Insert Source Point** lets you place a new vertex anywhere in
the drawing. With **Snap to Existing Points** on, clicking near an existing
vertex or point copies its XYZ. A free click uses the height interpolated from
the active segment. To add a point at either end, choose an endpoint of an open
line and use **Extend Source Line**. An ordinary line becomes a 3D polyline
when it gains a vertex. Arc insertion and extension use chord sampling.

**Draw Source Line** starts a new linked 3D polyline. Click its points or enter
XYZ values. **Finish** or Enter attaches the line to the surface. Cancel
discards the temporary preview.

**Delete Source Line** deletes the selected chain from the drawing and the
surface. **Remove Point** reconnects the neighbours of a deleted polyline vertex
or deletes an isolated source point. A line or arc with an endpoint removed
becomes a point at the opposite end.
Editing an arc endpoint, or assigning an individual elevation to a vertex of
a 2D polyline, converts the curve to a 3D polyline using sampled chord geometry
for its curved segments. The edits are undoable. The mesh and conflict flags
update with the source, and a resolved conflict loses its flag.

**Show Linked Triangles** controls the generated triangle layer. The source
overlay can show the lines above the surface at a configurable screen width
in plan and orbit without changing their drawn properties in the file.

**Remove Triangles Along Line** removes the triangles whose interiors the
temporary path crosses in plan. Click the path's points and press **Apply Line**
or Enter. Draw another path immediately, or choose **Stop** or press Esc to
finish. Each applied path can be undone individually. A linked surface stores
the paths in the drawing and reapplies them when rebuilding.
**Restore Triangles Along Line** shows the hidden faces and lets you draw paths
to restore just the faces whose interiors they cross. This tool stays active
between paths too. Where the paths overlap, the latest applied path determines
whether a triangle is visible.
**Show Removed Triangles** previews the affected faces of a linked surface
without editing the drawing. Pick a removal line to see the faces it can
restore, then choose **Restore Selected Removal**. **Restore Latest Removal**
removes the most recent cut from the history when viewing all removal lines.
Other paths remain in effect.
