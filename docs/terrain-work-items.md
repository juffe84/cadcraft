# Terrain work items for issue #3361

The first increments use ordinary drawing entities. A TIN is a named group of
triangular 3DFACEs in model space. Work can be reviewed and tested independently.

| Work item | Status | Acceptance check |
| --- | --- | --- |
| Build terrain from points and breaklines | Implemented | Select CAD POINTs, lines and 3D polylines or pass XYZ paths and outer or hole boundaries to `TIN`. The output preserves sample heights and constrained edges. |
| Generate terrain contours | Implemented | `TIN.CONTOURS` writes joined 3D polylines and can label index contours. Verify a contour crossing two triangles remains continuous. |
| Compare existing and design terrain | Implemented | `TIN.VOLUMES` compares two named groups and reports cut, fill, net fill and overlapping area in drawing units. |
| Persist the triangle surface | Implemented as standard CAD entities | Save as DWG or DXF and reopen with the grouped faces and their coordinates intact. |
| Edit terrain points and constraints | Pending | Add, remove or change a sample and rebuild neighboring triangles while keeping constraints. |
| Add terrain analysis and display styles | Pending | Provide elevation and slope bands, slope arrows, watershed analysis and selectable display of triangles, contours and points. |
| LandXML surface interchange | Pending | Round-trip named TIN vertices, faces, units and boundaries in LandXML. |
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
